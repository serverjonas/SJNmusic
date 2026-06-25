//! Top-level view dispatcher. Each route has its own module that
//! exports `pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp)`.
//! Keeping the dispatcher here means every route still sees a mutable
//! `&mut SJNMusicApp` so it can reach per-view scratch state (cached
//! fetched songs, in-flight flags, etc.) without inventing a per-view
//! state machine.

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::state::Route;

pub mod library;
pub mod queue;
pub mod playlists;
pub mod playlist;
pub mod search_download;
pub mod downloads;
pub mod history;
pub mod stats;
pub mod settings;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    // Clone the route so we can drop the borrow before calling module
    // functions that re-borrow `app`. Cheap (Route is small, Clone is
    // a String clone only for Playlist(name)).
    let route = app.route.clone();
    match route {
        Route::Library => library::show(ui, app),
        Route::Queue => queue::show(ui, app),
        Route::Playlists => playlists::show(ui, app),
        Route::Playlist(name) => playlist::show(ui, app, &name),
        Route::SearchDownload => search_download::show(ui, app),
        Route::Downloads => downloads::show(ui, app),
        Route::History => history::show(ui, app),
        Route::Stats => stats::show(ui, app),
        Route::Settings => settings::show(ui, app),
    }
}
