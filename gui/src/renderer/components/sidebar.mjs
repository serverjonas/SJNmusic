// Sidebar: navigation + offline banner. Re-renders whenever the store
// reports `online` toggling, since the banner belongs here.

import { store } from '../store.mjs';

const ROUTES = [
  { hash: '#library',    label: 'Library',    icon: '🎵' },
  { hash: '#queue',      label: 'Queue',      icon: '📜' },
  { hash: '#playlists',  label: 'Playlists',  icon: '➕' },
  { hash: '#search',     label: 'Search + Download', icon: '🔎' },
  { hash: '#downloads',  label: 'Downloads',  icon: '⬇' },
  { hash: '#history',    label: 'History',    icon: '🕓' },
  { hash: '#stats',      label: 'Stats',      icon: '📊' },
  { hash: '#settings',   label: 'Settings',   icon: '⚙' },
];

export function register() {
  if (customElements.get('app-sidebar')) return;
  customElements.define('app-sidebar', class extends HTMLElement {
    connectedCallback() {
      this._unsub = store.subscribe(() => this.render());
      this.render();
    }
    disconnectedCallback() {
      this._unsub?.();
    }
    render() {
      const { online, route, error } = store.state;
      const navHtml = ROUTES.map(r => `
        <a class="nav-link ${route === r.hash ? 'active' : ''}" href="${r.hash}">
          <span aria-hidden="true">${r.icon}</span>
          <span class="nav-label">${r.label}</span>
        </a>
      `).join('');
      const banner = online ? '' :
        `<div class="offline-banner" title="${(error || 'daemon unreachable').replace(/"/g, '&quot;')}">
          daemon offline
        </div>`;
      this.innerHTML = `
        <div class="brand">sjnmusic</div>
        ${banner}
        <nav>${navHtml}</nav>
      `;
    }
  });
}
