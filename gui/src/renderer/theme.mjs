// Theme management. Only two values: 'dark' (default) and 'light'. We
// store the choice in localStorage AND sync to the daemon HTTP read at
// startup if the daemon is reachable (deferred for v1).

import { store } from './store.mjs';

export const THEMES = ['dark', 'light'];

export function applyTheme(name) {
  if (!THEMES.includes(name)) name = 'dark';
  document.body.dataset.theme = name;
  store.set({ theme: name });
  try { localStorage.setItem('sjnmusic.theme', name); } catch { /* ignore */ }
}

export function cycleTheme() {
  applyTheme(store.state.theme === 'dark' ? 'light' : 'dark');
}
