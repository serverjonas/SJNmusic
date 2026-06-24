// Toast host: small notification banner overlay. Listens to store.toasts.

import { store } from '../store.mjs';

export function register() {
  if (customElements.get('app-toast-host')) return;
  customElements.define('app-toast-host', class extends HTMLElement {
    connectedCallback() {
      this._unsub = store.subscribe(() => this.render());
      this.render();
    }
    disconnectedCallback() { this._unsub?.(); }
    render() {
      const items = (store.state.toasts || [])
        .map(t => `<div class="toast ${t.level}">${escapeHtml(t.message)}</div>`)
        .join('');
      this.innerHTML = items || '';
    }
  });
  function escapeHtml(s) {
    return String(s||'').replace(/[<>&"]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;'}[c]));
  }
}
