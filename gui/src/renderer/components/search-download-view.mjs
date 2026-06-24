// Search + Download view: text box → /search/yt → numbered cards with
// thumbnails, click to download via /init.

export function register() {
  if (customElements.get('app-search-download-view')) return;
  customElements.define('app-search-download-view', class extends HTMLElement {
    connectedCallback() { this.render(); }

    async render() {
      this.innerHTML = `
        <div class="view-header">
          <h1>Search + Download</h1>
          <input id="sd-q" class="view-search" type="search"
                 placeholder="Search YouTube via yt-dlp…">
          <input id="sd-n" type="number" min="1" max="20" value="3"
                 title="candidates to show (1-20)" style="width:70px;">
          <button data-act="go">Search</button>
        </div>
        <div id="sd-results" class="search-results"></div>
      `;
      this.querySelector('[data-act="go"]').addEventListener('click', () => this.runSearch());
      this.querySelector('#sd-q').addEventListener('keydown', (e) => {
        if (e.key === 'Enter') this.runSearch();
      });
      this.querySelector('#sd-q').focus();
    }

    async runSearch(reason) {
      const q = this.querySelector('#sd-q').value.trim();
      const n = parseInt(this.querySelector('#sd-n').value, 10) || 3;
      const grid = this.querySelector('#sd-results');
      if (!q) { grid.innerHTML = `<div class="meta">Type a query above.</div>`; return; }
      grid.innerHTML = `<div class="meta">Searching yt-dlp…</div>`;
      const r = await window.api.get('/search/yt', { q, limit: n });
      if (!r.ok) { grid.innerHTML = `<div class="meta">error: ${escapeHtml(r.error || 'failed')}</div>`; return; }
      const results = r.data?.results || [];
      if (results.length === 0) {
        grid.innerHTML = `<div class="meta">No results.</div>`;
        return;
      }
      grid.innerHTML = results.map((c, i) => `
        <button class="candidate" data-i="${i}">
          ${c.thumbnail
            ? `<img class="thumb" src="${escapeAttr(c.thumbnail)}" referrerpolicy="no-referrer" alt="" loading="lazy">`
            : `<span class="thumb" style="display:flex;align-items:center;justify-content:center;font-size:24px;">🎵</span>`}
          <div class="info">
            <div class="t">${escapeHtml(c.title)}</div>
            <div class="u">${escapeHtml(c.uploader || '(unknown artist)')}</div>
            <div class="d">${fmtSecs(c.duration_secs)}</div>
          </div>
        </button>`).join('');
      grid.querySelectorAll('.candidate').forEach(el => {
        el.addEventListener('click', async () => {
          const i = parseInt(el.dataset.i, 10);
          const c = results[i];
          grid.innerHTML = `<div class="meta">Downloading…</div>`;
          const dr = await window.api.post('/init', { body: { name: q, url: c.url } });
          if (!dr.ok) {
            grid.innerHTML = `<div class="meta">error: ${escapeHtml(dr.error || 'failed')}</div>`;
            return;
          }
          grid.innerHTML = `
            <div class="stats-card">
              <div class="label">queued</div>
              <div class="value">job ${dr.data.job_id}</div>
              <div class="meta">Watching Downloads for completion…</div>
            </div>`;
        });
      });
    }
  });
  function escapeHtml(s) { return String(s||'').replace(/[<>&"]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;'}[c])); }
  function escapeAttr(s) { return String(s||'').replace(/"/g, '&quot;'); }
  function fmtSecs(s) {
    s = Math.max(0, Math.round(s|0));
    const m = Math.floor(s/60), r = s % 60;
    return `${m}:${r.toString().padStart(2,'0')}`;
  }
}
