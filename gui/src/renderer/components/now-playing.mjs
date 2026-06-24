// Footer transport strip: title, transport buttons, volume slider, repeat
// selector. Re-renders on every tick.

import { store } from '../store.mjs';
import { fmtSecs } from '../fmt.mjs';

async function post(path, body) {
  try {
    const r = await window.api.post(path, body ? { body } : undefined);
    if (!r.ok && r.error) console.warn(`POST ${path}`, r.error);
  } catch (e) {
    console.warn(`POST ${path} failed`, e);
  }
}

export function register() {
  if (customElements.get('app-now-playing')) return;
  customElements.define('app-now-playing', class extends HTMLElement {
    connectedCallback() {
      this._unsub = store.subscribe(() => this.render());
      this.render();
    }
    disconnectedCallback() {
      this._unsub?.();
    }
    fmt(s) { return fmtSecs(s); }

    render() {
      const np = store.state.nowPlaying || {};
      const cur = np.current || {};
      const title = cur.name || '(nothing playing)';
      const elapsed = np.elapsed_secs ?? 0;
      const raw_dur = np.duration_secs;
      const duration = (raw_dur && raw_dur > 0) ? raw_dur : null;
      const paused = !!np.paused;
      const playing = !!np.playing;
      const vol = np.volume ?? 1.0;
      const repeat = np.repeat || 'off';

      this.innerHTML = `
        <div class="np-current">
          ${title.replace(/[<>&]/g, c => ({'<':'&lt;','>':'&gt;','&':'&amp;'}[c]))}
          <small>${paused ? 'paused' : (playing ? 'playing' : 'idle')}</small>
        </div>
        <div class="np-controls">
          <button class="icon-btn" data-act="seek-back">⟲10s</button>
          <button class="icon-btn" data-act="toggle">${paused || !playing ? '▶' : '⏸'}</button>
          <button class="icon-btn" data-act="skip">⏭</button>
          <select data-act="repeat" title="Repeat">
            <option value="off" ${repeat==='off'?'selected':''}>off</option>
            <option value="one" ${repeat==='one'?'selected':''}>one</option>
            <option value="all" ${repeat==='all'?'selected':''}>all</option>
          </select>
        </div>
        <div class="np-progress">
          <span class="np-time">${this.fmt(elapsed)} / ${this.fmt(duration)}</span>
          <div class="np-volume">
            <label>vol</label>
            <input type="range" min="0" max="2" step="0.01" value="${vol}" data-act="volume">
          </div>
        </div>
      `;

      this.querySelector('[data-act="toggle"]')?.addEventListener('click',
        () => paused ? post('/resume') : post('/pause'));
      this.querySelector('[data-act="skip"]')?.addEventListener('click',
        () => post('/skip'));
      this.querySelector('[data-act="seek-back"]')?.addEventListener('click',
        async () => {
          const newPos = Math.max(0, (elapsed - 10));
          await post('/seek', { secs: newPos });
        });
      this.querySelector('[data-act="repeat"]')?.addEventListener('change',
        (e) => post('/repeat', { mode: e.target.value }));
      this.querySelector('[data-act="volume"]')?.addEventListener('input',
        (e) => post('/volume', { vol: parseFloat(e.target.value) }));
    }
  });
}
