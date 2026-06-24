# sjnmusic-gui

Electron-based remote control for `sjnmusicd`. Pure HTTP client — the daemon
owns the audio engine, this GUI just renders state and fires commands.

## Run from source

```bash
cd gui
npm install
npm start
```

## Build distributables (Linux)

```bash
npm install
npm run build:linux   # AppImage + .deb in gui/out/
```

## Architecture

```
gui/src/
  main/         Electron main process (BrowserWindow, polling, tray)
  preload/      contextBridge IPC → window.api.{get,post,patch,put,del,onTick}
  renderer/     Web Components (no framework, hand-rolled CSS)
```

The daemon stays untouched; the only additions are:
- `YtCandidate.thumbnail: Option<String>` (for picker cover art),
- `daemon/src/mpris.rs` registering `org.mpris.MediaPlayer2.sjnmusic` on the
  session bus so media keys work without the GUI open.

State is polled from the main process at 1 Hz and pushed into the renderer
via `webContents.send('tick', …)`.
