// Stats view: aggregate cards from /stats plus top songs.

import { fmtSecs, fmtBigSecs, escapeHtml } from '../fmt.mjs';

export function register() {
  if (customElements.get('app-stats-view')) return;
  customElements.define('app-stats-view', class extends HTMLElement {
    connectedCallback() { this.render(); }
    async render() {
      let stats = { total_plays: 0, total_secs: 0, top_songs: [] };
      try {
        const r = await window.api.get('/stats');
        if (r.ok) stats = r.data || stats;
      } catch (e) {}
      const cards = `
        <div class="stats-grid">
          <div class="stats-card">
            <div class="label">Total plays</div>
            <div class="value">${stats.total_plays || 0}</div>
          </div>
          <div class="stats-card">
            <div class="label">Total time</div>
            <div class="value">${fmtBigSecs(stats.total_secs || 0)}</div>
          </div>
        </div>`;
      const top = (stats.top_songs || []).map((t, i) => `
        <div class="row">
          <span class="meta">${i + 1}</span>
          <div>
            <div class="title">${escapeHtml(t.name || '(missing)')}</div>
            <div class="meta">id ${t.song_id}</div>
          </div>
          <span class="meta">${t.plays} plays</span>
          <span class="meta">${fmtSecs(t.total_secs || 0)}</span>
        </div>`).join('') || `<div class="meta">No plays yet.</div>`;
      this.innerHTML = `
        <div class="view-header"><h1>Stats</h1></div>
        ${cards}
        <h2 style="margin-top:20px;">Top songs</h2>
        <div class="row-list">${top}</div>
      `;
    }
  });
}
