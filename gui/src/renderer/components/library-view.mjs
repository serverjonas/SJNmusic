// Library view: list of songs plus a fuzzy search box. Click to play.

import { store } from '../store.mjs';

export function register() {
  if (customElements.get('app-library-view')) return;
  customElements.define('app-library-view', class extends HTMLElement {
    connectedCallback() { this.render(); }
    render() {
      this.innerHTML = `
        <div class="view-header">
          <h1>Library</h1>
          <input class="view-search" type="search"
                 placeholder="Filter by name (fuzzy)"
                 ${this.onsearch?.() ? '' : 'id="lib-q"'}>
        </div>
        <div id="lib-root" class="row-list"></div>
      `;
      const root = this.querySelector('#lib-root');
      const qBox = this.querySelector('#lib-q');
      const fetchOnce = async () => {
        const q = qBox?.value?.trim() || '';
        const r = q
          ? await window.api.get('/search', { q })
          : await window.api.get('/songs');
        const songs = q ? [r.data] : (r.data?.songs || []);
        const items = songs
          .filter(s => s && (s.id !== undefined))
          .map(s => {
            const dur = s.duration_secs ? ` · ${this._fmt(s.duration_secs)}` : '';
            return `
              <div class="row" data-id="${s.id}" data-name="${(s.name||'').replace(/"/g,'&quot;')}">
                <span class="meta">▶</span>
                <div>
                  <div class="title">${(s.name||'').replace(/[<>&]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;'}[c]))}</div>
                  <div class="meta">id ${s.id}${dur}</div>
                </div>
                <button class="icon-btn" data-act="queue-add" title="Add to queue">+queue</button>
                <button class="icon-btn danger" data-act="delete" title="Delete">delete</button>
              </div>`;
          }).join('') || `<div class="meta">No songs yet — head to Search + Download.</div>`;
        root.innerHTML = items;
        root.querySelectorAll('.row').forEach(el => {
          el.addEventListener('click', (ev) => {
            const tgt = ev.target;
            if (tgt?.dataset?.act === 'queue-add') {
              window.api.post('/add', { body: { query: el.dataset.name } });
            } else if (tgt?.dataset?.act === 'delete') {
              window.api.del('/del', { body: { query: el.dataset.name } });
            } else {
              window.api.post('/play', { body: { query: el.dataset.name } });
            }
          });
        });
      };
      qBox?.addEventListener('input', debounce(fetchOnce, 180));
      fetchOnce();
    }
    _fmt(s) {
      s = Math.max(0, Math.round(s|0));
      const m = Math.floor(s/60), r = s % 60;
      return `${m}:${r.toString().padStart(2,'0')}`;
    }
  });

  function debounce(fn, ms) {
    let t;
    return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
  }
}
