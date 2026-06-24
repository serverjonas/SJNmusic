// Playlist list view: shows all playlists; create via prompt.

export function register() {
  if (customElements.get('app-playlist-list-view')) return;
  customElements.define('app-playlist-list-view', class extends HTMLElement {
    connectedCallback() { this.render(); }
    async render() {
      let playlists = [];
      try {
        const r = await window.api.get('/playlists');
        playlists = r.ok ? (r.data || []) : [];
      } catch (e) { playlists = []; }
      const items = playlists.map(pl => `
        <div class="row" data-name="${escapeHtml(pl.name)}">
          <span class="meta">▶</span>
          <div>
            <div class="title">${escapeHtml(pl.name)}</div>
            <div class="meta">${(pl.songs || []).length} songs</div>
          </div>
          <button class="icon-btn" data-act="go">open</button>
          <button class="icon-btn danger" data-act="rm">delete</button>
        </div>`).join('') || `<div class="meta">No playlists yet.</div>`;
      this.innerHTML = `
        <div class="view-header">
          <h1>Playlists</h1>
          <input id="pl-new" type="text" placeholder="new playlist name" style="flex:1;">
          <button data-act="create">Create</button>
        </div>
        <div class="row-list">${items}</div>
      `;
      this.querySelector('[data-act="create"]')?.addEventListener('click', async () => {
        const name = this.querySelector('#pl-new')?.value?.trim();
        if (!name) return;
        const r = await window.api.post('/playlists', { body: { name } });
        if (r.ok) location.hash = `#playlist/${encodeURIComponent(name)}`;
        else console.warn('create playlist failed', r.error);
      });
      this.querySelectorAll('.row').forEach(el => {
        el.addEventListener('click', (ev) => {
          const act = ev.target?.dataset?.act;
          if (act === 'go')  location.hash = `#playlist/${encodeURIComponent(el.dataset.name)}`;
          else if (act === 'rm') window.api.del(`/playlists/${encodeURIComponent(el.dataset.name)}`);
        });
      });
    }
  });
  function escapeHtml(s) { return String(s||'').replace(/[<>&"]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;'}[c])); }
}
