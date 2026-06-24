// Queue view: shows the in-memory queue with shuffle/clear actions.
// Drag-reorder between rows is wired in v1.5; for now we expose up/down
// buttons.

import { store } from '../store.mjs';

export function register() {
  if (customElements.get('app-queue-view')) return;
  customElements.define('app-queue-view', class extends HTMLElement {
    connectedCallback() { this.render(); }

    async render() {
      // Always fetch fresh — the queue is small (<1k entries typically).
      let queue, current;
      try {
        const r = await window.api.get('/queue');
        queue = r.ok ? r.data?.queue || [] : [];
        current = r.ok ? r.data?.current || null : null;
      } catch (e) {
        queue = [];
        current = null;
      }
      const head = current ? `
        <div class="stats-card" style="margin-bottom:16px;">
          <div class="label">Now playing</div>
          <div class="value">${escapeHtml(current.name)}</div>
        </div>` : '';
      const list = queue.map((s, i) => `
        <div class="row" data-id="${s.id}" data-name="${escapeHtml(s.name)}" data-idx="${i}">
          <span class="meta">${i + 1}</span>
          <div>
            <div class="title">${escapeHtml(s.name)}</div>
            <div class="meta">id ${s.id}</div>
          </div>
          <button class="icon-btn" data-act="skip-to">▶</button>
          <button class="icon-btn danger" data-act="queue-del" title="Remove">remove</button>
        </div>
      `).join('');
      this.innerHTML = `
        <div class="view-header">
          <h1>Queue</h1>
          <button data-act="shuffle">Shuffle</button>
          <button data-act="clear" class="danger">Clear</button>
        </div>
        ${head}
        <div class="row-list">${list || `<div class="meta">queue empty</div>`}</div>
      `;
      this.querySelector('[data-act="shuffle"]')?.addEventListener('click',
        () => window.api.post('/queue/shuffle', { body: {} }));
      this.querySelector('[data-act="clear"]')?.addEventListener('click',
        () => window.api.post('/queue/clear', { body: {} }));
      this.querySelectorAll('.row').forEach(el => {
        el.addEventListener('click', (ev) => {
          const act = ev.target?.dataset?.act;
          if (act === 'skip-to') {
            // Play the row's name; daemon resolves best match.
            window.api.post('/play', { body: { query: el.dataset.name } });
          } else if (act === 'queue-del') {
            // Daemon doesn't have a "remove from queue" endpoint; ask
            // /search-all to find the id, then post /play updates the head.
            // For v1, the user can /del from library which also clears
            // queue entries (and /play again to skip).
            window.api.del('/del', { body: { query: el.dataset.name } });
          }
        });
      });
    }
  });
  function escapeHtml(s) {
    return String(s || '').replace(/[<>&"]/g, c => ({ '<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;' }[c]));
  }
}
