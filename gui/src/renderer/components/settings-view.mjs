// Settings view: ONLY the light/dark theme picker, as requested.

import { store } from '../store.mjs';
import { applyTheme, THEMES } from '../theme.mjs';

export function register() {
  if (customElements.get('app-settings-view')) return;
  customElements.define('app-settings-view', class extends HTMLElement {
    connectedCallback() { this.render(); }
    render() {
      const buttons = THEMES.map(t => `
        <button data-t="${t}" class="${store.state.theme === t ? 'active' : ''}">
          ${t === 'dark' ? '🌙 Dark' : '☀️ Light'}
        </button>`).join('');
      this.innerHTML = `
        <div class="view-header">
          <h1>Settings</h1>
        </div>
        <div class="stats-card">
          <div class="label">Theme</div>
          <div class="theme-picker" style="margin-top:8px;">${buttons}</div>
          <p class="meta" style="margin-top:10px;">
            Default is dark. Choice persists per-machine (localStorage).
            The daemon is unchanged — the GUI reads its theme locally.
          </p>
        </div>
        <div class="stats-card" style="margin-top:16px;">
          <div class="label">Daemon</div>
          <p class="meta">The GUI is a remote control. It does not own audio.
                Edit <code>~/.sjn/music/config.toml</code> and restart the daemon
                to change host/port, library paths, fuzzy threshold, …</p>
        </div>
      `;
      this.querySelectorAll('[data-t]').forEach(el => {
        el.addEventListener('click', () => applyTheme(el.dataset.t));
      });
    }
  });
}
