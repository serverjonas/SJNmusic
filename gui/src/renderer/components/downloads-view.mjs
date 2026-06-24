// Downloads view: live status table for in-flight yt-dlp jobs. Re-fetches
// every 3 seconds while this view is in the foreground (and immediately
// on every model tick from main, which is cheap enough).

import { store } from '../store.mjs';

export function register() {
  if (customElements.get('app-downloads-view')) return;
  customElements.define('app-downloads-view', class extends HTMLElement {
    connectedCallback() {
      this.render();
      this._timer = setInterval(() => this.render(), 4000);
    }
    disconnectedCallback() {
      clearInterval(this._timer);
    }

    async render() {
      // Prefer the snapshot pushed by main; fall back to a fresh fetch
      // if it's missing.
      let dl = store.state.downloads?.downloads;
      if (!dl) {
        const r = await window.api.get('/downloads');
        dl = r.ok ? (r.data?.downloads || []) : [];
      }
      const rows = dl.map(d => `
        <tr>
          <td>${d.id ?? '-'}</td>
          <td><span class="badge ${d.status}">${escapeHtml(d.status || '?')}</span></td>
          <td>${escapeHtml(d.name || '')}</td>
          <td class="meta">${escapeHtml(d.source || '')}</td>
          <td class="meta">${d.song_id ? `song ${d.song_id}` : ''}</td>
          <td class="meta">${d.error ? escapeHtml(d.error) : ''}</td>
        </tr>
      `).join('') || `<tr><td colspan="6" class="meta">No jobs.</td></tr>`;
      this.innerHTML = `
        <div class="view-header">
          <h1>Downloads</h1>
          <button data-act="refresh">Refresh now</button>
        </div>
        <table class="downloads-table" style="width:100%;border-collapse:collapse;">
          <thead>
            <tr><th>id</th><th>status</th><th>name</th><th>source</th><th>result</th><th>error</th></tr>
          </thead>
          <tbody>${rows}</tbody>
        </table>
      `;
      this.querySelector('[data-act="refresh"]').addEventListener('click', () => this.render());
    }
  });
  function escapeHtml(s) { return String(s||'').replace(/[<>&"]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;'}[c])); }
}
