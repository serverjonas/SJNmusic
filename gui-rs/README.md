# sjnmusic GUI (Rust)

Pure-Rust remote control for `sjnmusicd`. Built on
[`egui`](https://github.com/emilk/egui) + [`eframe`](https://github.com/emilk/egui/tree/master/crates/eframe).
Talks to the daemon over HTTP at `127.0.0.1:14567` by default and owns no
audio state.

This replaces the previous Electron GUI. The daemon API is unchanged.

## Build & Run

Prerequisites: Rust 1.80+ (stable). On Linux you may also need:
- `libssl-dev` if you ever swap `ureq` for `reqwest` (not needed today)
- `libdbus-1-dev` + a system tray daemon (e.g. `libappindicator`,
  KDE StatusNotifierItem watcher, GNOME Shell extension) for the optional
  tray icon. The tray fails gracefully if one is missing.

```sh
cd gui-rs
cargo run --release
```

To launch with the system tray enabled the GUI runs the tray at every
startup, but it falls back silently on systems without a tray service, so
it's safe to leave it on.

### Environment variables

| Variable            | Default     | Meaning                                            |
|---------------------|-------------|----------------------------------------------------|
| `SJNMUSIC_HOST`     | `127.0.0.1` | Daemon host                                        |
| `SJNMUSIC_PORT`     | `14567`     | Daemon port                                        |
| `SJNMUSIC_THEME`    | `dark`      | `dark` or `light` (also persisted to disk)         |
| `RUST_LOG`          | `info`      | Standard env_logger filter                         |

## Features (feature parity with the Electron GUI)

- Sidebar with 8 nav routes + offline banner
- Now-playing footer with transport, seek-back, repeat, volume
- Library view with fuzzy-search filter
- Queue view with shuffle / clear / skip-to / remove
- Playlists: list view + single playlist view (add/remove/reorder/
  rename/duplicate/delete/play)
- Search + Download with yt-dlp candidate picker and inline thumbnails
- Downloads: live status table auto-refresh
- History (last 100 plays)
- Stats with cards and top songs
- Settings (theme picker)
- Spacebar = pause/resume
- System tray (best effort): now-playing label, pause/resume, skip,
  show window, quit

## Architecture

```
src/
  main.rs           entry: eframe::run_native, tray bootstrap
  app.rs            SJNMusicApp: eframe::App impl, panel layout, router
  state.rs          AppState, polling worker, Route enum, Theme
  daemon.rs         HTTP client + typed daemon responses
  fmt.rs            formatting helpers (time, html escape)
  tray.rs           tray-icon setup (graceful fallback)
  thumbnails.rs     async thumbnail download + TextureHandle cache
  views/
    mod.rs          dispatch helper
    library.rs      fuzzy-filtered song list
    queue.rs        current queue + shuffle / clear
    playlists.rs    list view + create
    playlist.rs     single playlist (add/remove/reorder/rename/...)
    search_download.rs   yt-dlp picker + thumbnails
    downloads.rs    live status table
    history.rs      recent plays
    stats.rs        aggregate cards + top songs
    settings.rs     theme picker
```

The UI thread owns an `Arc<DaemonClient>` and a background worker thread
polls `/now-playing`, `/queue`, and `/downloads` every second, pushing
snapshots into `Arc<Mutex<Snapshot>>`. After each poll the worker calls
`ctx.request_repaint()` so the UI picks up changes.

UI actions (play / pause / skip / volume slider / ...) run as
fire-and-forget `std::thread::spawn`s calling `DaemonClient` directly,
so a stalled daemon never freezes the GUI.
