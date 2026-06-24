// Single playlist view: songs in a list, drag-reorder in v1.5, with
// up/down buttons for now. Rename + duplicate via header buttons.

export function register() {
  if (customElements.get('app-playlist-view')) return;
  customElements.define('app-playlist-view', class extends HTMLElement {
    static get observedAttributes() { return ['name']; }

    async connectedCallback() { this.render(); }
    async attributeChangedCallback() { this.render(); }

    async render() {
      const name = this.getAttribute('name');
      if (!name) {
        this.innerHTML = `<div class="view-header"><h1>Playlist</h1></div>
                          <div class="meta">No playlist selected.</div>`;
        return;
      }
      let pl = null;
      try {
        const r = await window.api.get(`/playlists/${encodeURIComponent(name)}`);
        if (r.ok) pl = r.data;
      } catch (e) { pl = null; }
      const songs = pl?.songs || [];
      const items = songs.map((s, i) => `
        <div class="row" data-id="${s.id}" data-name="${escapeHtml(s.name)}" data-idx="${i}">
          <span class="meta">${i + 1}</span>
          <div>
            <div class="title">${escapeHtml(s.name)}</div>
            <div class="meta">id ${s.id}</div>
          </div>
          <button class="icon-btn" data-act="up" title="Move up">↑</button>
          <button class="icon-btn" data-act="dn" title="Move down">↓</button>
          <button class="icon-btn danger" data-act="rm">remove</button>
        </div>`).join('') || `<div class="meta">empty playlist</div>`;
      this.innerHTML = `
        <div class="view-header">
          <h1>${escapeHtml(name)}</h1>
          <input id="pl-add-q" type="search" placeholder="add a song by name…" style="flex:1;">
          <button data-act="add">Add</button>
          <button data-act="play">Play</button>
          <button data-act="rename">Rename</button>
          <button data-act="duplicate">Duplicate</button>
          <button data-act="delete" class="danger">Delete</button>
        </div>
        <div class="row-list">${items}</div>
      `;

      const call = (method, path, body) => window.api[method](path, body ? { body } : undefined);

      this.querySelector('[data-act="add"]')?.addEventListener('click', async () => {
        const q = this.querySelector('#pl-add-q')?.value?.trim();
        if (!q) return;
        await call('post', `/playlists/${encodeURIComponent(name)}/add`, { query: q });
        this.render();
      });
      this.querySelector('[data-act="play"]')?.addEventListener('click',
        () => call('post', `/playlists/${encodeURIComponent(name)}/play`));
      this.querySelector('[data-act="delete"]')?.addEventListener('click', async () => {
        if (!confirm(`Delete playlist "${name}"?`)) return;
        await call('del', `/playlists/${encodeURIComponent(name)}`);
        location.hash = '#playlists';
      });
      this.querySelector('[data-act="rename"]')?.addEventListener('click', async () => {
        const new_name = prompt('New name:', name);
        if (!new_name || new_name === name) return;
        await call('patch', `/playlists/${encodeURIComponent(name)}`, { name: new_name });
        location.hash = `#playlist/${encodeURIComponent(new_name)}`;
      });
      this.querySelector('[data-act="duplicate"]')?.addEventListener('click', async () => {
        const new_name = prompt('Duplicate as:', `${name} (copy)`);
        if (!new_name) return;
        await call('post', `/playlists/${encodeURIComponent(name)}/duplicate`, { name: new_name });
        location.hash = `#playlist/${encodeURIComponent(new_name)}`;
      });
      this.querySelectorAll('.row').forEach(el => {
        el.addEventListener('click', async (ev) => {
          const act = ev.target?.dataset?.act;
          if (act === 'rm') {
            await call('del', `/playlists/${encodeURIComponent(name)}/songs/${el.dataset.id}`);
            this.render();
          } else if (act === 'up' || act === 'dn') {
            const from = parseInt(el.dataset.idx, 10) + 1;
            const to = from + (act === 'up' ? -1 : 1);
            if (to < 1 || to > songs.length) return;
            await call('post', `/playlists/${encodeURIComponent(name)}/reorder`, { from, to });
            this.render();
          } else {
            window.api.post('/play', { body: { query: el.dataset.name } });
          }
        });
      });
    }
  });
  function escapeHtml(s) { return String(s||'').replace(/[<>&"]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;'}[c])); }
}
