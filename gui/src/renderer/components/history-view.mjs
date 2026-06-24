// History view: most-recent 100 entries.

export function register() {
  if (customElements.get('app-history-view')) return;
  customElements.define('app-history-view', class extends HTMLElement {
    connectedCallback() { this.render(); }
    async render() {
      let hist = [];
      try {
        const r = await window.api.get('/history', { limit: 100 });
        hist = r.ok ? (r.data?.history || []) : [];
      } catch (e) {}
      const rows = hist.map(h => {
        const date = new Date((h.played_at || 0) * 1000);
        const when = isNaN(date.getTime()) ? '(no date)' : date.toLocaleString();
        return `
          <div class="row">
            <span class="meta">${escapeHtml(when)}</span>
            <div>
              <div class="title">${escapeHtml(h.song_name || '(missing song)')}</div>
              <div class="meta">id ${h.song_id}</div>
            </div>
            <span class="meta">${fmtSecs(h.duration_secs_played || 0)}</span>
            <span></span>
          </div>`;
      }).join('') || `<div class="meta">No history yet.</div>`;
      this.innerHTML = `
        <div class="view-header"><h1>History</h1></div>
        <div class="row-list">${rows}</div>
      `;
    }
  });
  function escapeHtml(s) { return String(s||'').replace(/[<>&"]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;'}[c])); }
  function fmtSecs(s) { s = Math.max(0, Math.round(s|0)); const m = Math.floor(s/60), r = s%60; return `${m}:${r.toString().padStart(2,'0')}`; }
}
