// Electron main entrypoint. Owns the DaemonClient, polls /now-playing +
// /queue + /downloads on a 1s timer, forwards the snapshot to the renderer.
// Also owns the optional system tray.

import { app, BrowserWindow, ipcMain, Menu } from 'electron';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { DaemonClient } from './daemon.mjs';
import { installHandlers } from './ipc.mjs';
import { buildTray } from './tray.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const PRELOAD = path.join(__dirname, '../preload/preload.mjs');
const RENDERER = path.join(__dirname, '../renderer/index.html');

const POLL_INTERVAL_MS = 1000;

// Single-instance lock so launching `sjnmusic-gui` twice focuses the
// existing window instead of starting a second Chromium process.
const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
  process.exit(0);
}

const daemon = new DaemonClient({
  host: process.env.SJNMUSIC_HOST || '127.0.0.1',
  port: parseInt(process.env.SJNMUSIC_PORT || '14567', 10),
});

let mainWindow = null;
let tray = null;
let pollTimer = null;

function emitTick(data) {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send('tick', data);
  }
}

async function pollOnce() {
  try {
    const [nowPlaying, queue, downloads] = await Promise.all([
      daemon.get('/now-playing'),
      daemon.get('/queue'),
      daemon.get('/downloads'),
    ]);
    emitTick({
      online: true,
      nowPlaying,
      queue,
      downloads,
      polledAt: Date.now(),
    });
  } catch (err) {
    emitTick({
      online: false,
      error: err.daemonResponse?.error || err.message,
      polledAt: Date.now(),
    });
  }
}

function startPolling() {
  // First poll immediately so the renderer doesn't sit on empty data.
  pollOnce();
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = setInterval(pollOnce, POLL_INTERVAL_MS);
}

function stopPolling() {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function createMainWindow() {
  mainWindow = new BrowserWindow({
    width: 1180,
    height: 760,
    minWidth: 820,
    minHeight: 520,
    show: false,
    backgroundColor: '#0f1115',
    title: 'sjnmusic',
    autoHideMenuBar: true,
    webPreferences: {
      preload: PRELOAD,
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      spellcheck: false,
    },
  });

  Menu.setApplicationMenu(null);
  mainWindow.removeMenu();

  mainWindow.once('ready-to-show', () => mainWindow.show());
  await mainWindow.loadFile(RENDERER);
}

app.on('second-instance', () => {
  if (!mainWindow) return;
  if (mainWindow.isMinimized()) mainWindow.restore();
  mainWindow.focus();
});

app.whenReady().then(async () => {
  installHandlers(ipcMain, daemon);
  await createMainWindow();
  tray = buildTray({ app, mainWindow: () => mainWindow, daemon });
  startPolling();

  app.on('activate', () => {
    if (!mainWindow) createMainWindow();
  });
});

app.on('window-all-closed', () => {
  // On Linux + macOS we keep the process alive so the daemon polling
  // continues; tray activation re-shows the window. On Windows we quit.
  if (process.platform === 'win32' && !tray) {
    app.quit();
  }
});

app.on('before-quit', () => {
  stopPolling();
});
