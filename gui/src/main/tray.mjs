// System tray icon. Built around Electron's `Tray` API. Some Linux setups
// (no libappindicator, Wayland without StatusNotifierItem) can refuse to
// construct a tray at all; we swallow that and return null so the rest of
// the app keeps running.

import { Menu, Tray, nativeImage } from 'electron';

/**
 * Synthesize a tiny 16x16 dark theme icon in memory, since we don't ship
 * image assets (and SVG rasterisation in Electron is unreliable). This
 * gives a monochrome square that's pleasant enough until someone adds a
 * real .png.
 */
function placeholderIcon() {
  // 16x16 imageData: a teal play-triangle on a dark background.
  const size = 16;
  const buf = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4;
      // Background: rgba(15,17,21,255)
      let r = 15, g = 17, b = 21, a = 255;
      // Triangle mask centred
      const cx = x - 7;
      const cy = y - 8;
      const inside = Math.abs(cx) <= cy && cy >= -7 && cy <= 6;
      if (inside) {
        r = 108; g = 178; b = 255; // accent
      }
      buf[i] = r; buf[i + 1] = g; buf[i + 2] = b; buf[i + 3] = a;
    }
  }
  return nativeImage.createFromBitmap(buf, { width: size, height: size });
}

export function buildTray({ app, mainWindow, daemon }) {
  try {
    const tray = new Tray(placeholderIcon());
    tray.setToolTip('sjnmusic');

    const rebuild = (state) => {
      const win = mainWindow();
      const showWin = () => win && !win.isDestroyed() && (win.isMinimized() ? win.restore() : win.show());

      const menu = Menu.buildFromTemplate([
        { label: state?.nowPlaying?.current?.name || 'sjnmusic', enabled: false },
        { type: 'separator' },
        {
          label: state?.nowPlaying?.paused ? 'Resume' : 'Pause',
          click: () => daemon.post(state?.nowPlaying?.paused ? '/resume' : '/pause'),
        },
        { label: 'Skip', click: () => daemon.post('/skip') },
        { type: 'separator' },
        { label: 'Show window', click: showWin },
        { label: 'Quit', click: () => { app.isQuitting = true; app.quit(); } },
      ]);
      tray.setContextMenu(menu);
    };

    rebuild({});

    // We poll the daemon for state snapshot inside main/index.mjs's loop
    // and forward via webContents. We mirror a tiny lastState here so the
    // menu reflects pause/resume. Index.mjs calls tray.maybeUpdate(state)
    // via the 'tick' channel — but since tray lives in main and we don't
    // want to give it the BrowserWindow, we just hook IPC internally.
    tray.maybeUpdate = rebuild;
    return tray;
  } catch (e) {
    console.warn('tray: failed to construct, continuing without:', e?.message || e);
    return null;
  }
}
