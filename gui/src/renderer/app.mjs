// Renderer entry point. Registers every Web Component, subscribes to
// `window.api.onTick`, and wires the hash-based router.

import { store } from './store.mjs';
import { applyTheme } from './theme.mjs';

import { register as regSidebar }      from './components/sidebar.mjs';
import { register as regNowPlaying }   from './components/now-playing.mjs';
import { register as regLibrary }      from './components/library-view.mjs';
import { register as regQueueV }       from './components/queue-view.mjs';
import { register as regPlaylistList } from './components/playlist-list-view.mjs';
import { register as regPlaylistV }    from './components/playlist-view.mjs';
import { register as regSelnDl }       from './components/search-download-view.mjs';
import { register as regDownloads }    from './components/downloads-view.mjs';
import { register as regHistory }      from './components/history-view.mjs';
import { register as regStats }        from './components/stats-view.mjs';
import { register as regSettings }     from './components/settings-view.mjs';
import { register as regToast }        from './components/toast.mjs';

// 1. Apply persisted theme before any component renders, so the first
//    paint doesn't flash the wrong colour.
applyTheme(store.state.theme);

// 2. Register all custom elements.
regSidebar();
regNowPlaying();
regLibrary();
regQueueV();
regPlaylistList();
regPlaylistV();
regSelnDl();
regDownloads();
regHistory();
regStats();
regSettings();
regToast();

// 3. Forward the polling ticks from main into the shared store.
window.api.onTick((snapshot) => {
  store.set({
    online: snapshot.online,
    nowPlaying: snapshot.nowPlaying,
    queue: snapshot.queue,
    downloads: snapshot.downloads,
    error: snapshot.error || null,
    polledAt: snapshot.polledAt,
  });
});

// 4. Minimal hash-based router. Recognised routes:
//    #library, #queue, #playlists, #playlist/<name>, #search,
//    #downloads, #history, #stats, #settings
function render() {
  const hash = location.hash || '#library';
  store.set({ route: hash });
  const main = document.getElementById('main-pane');
  if (!main) return;
  main.innerHTML = '';
  const [name, arg] = hash.replace(/^#/, '').split('/');
  switch (name) {
    case 'queue':
      main.appendChild(document.createElement('app-queue-view'));
      break;
    case 'playlists':
      main.appendChild(document.createElement('app-playlist-list-view'));
      break;
    case 'playlist':
      const el = document.createElement('app-playlist-view');
      if (arg) el.setAttribute('name', decodeURIComponent(arg));
      main.appendChild(el);
      break;
    case 'search':
      main.appendChild(document.createElement('app-search-download-view'));
      break;
    case 'downloads':
      main.appendChild(document.createElement('app-downloads-view'));
      break;
    case 'history':
      main.appendChild(document.createElement('app-history-view'));
      break;
    case 'stats':
      main.appendChild(document.createElement('app-stats-view'));
      break;
    case 'settings':
      main.appendChild(document.createElement('app-settings-view'));
      break;
    case 'library':
    default:
      main.appendChild(document.createElement('app-library-view'));
      break;
  }
}

window.addEventListener('hashchange', render);
render();

// 5. Keyboard shortcuts at the body level. We avoid stealing focus from
//    text inputs by checking document.activeElement.
document.addEventListener('keydown', async (e) => {
  const ae = document.activeElement;
  if (ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA')) return;
  if (e.code === 'Space') {
    e.preventDefault();
    await window.api.post('/pause').catch(() => {});
  }
});

// Inform the daemon we're alive — the daemon doesn't currently do anything
// with this; populate stats later if we add startup latency tracking.
window.api.get('/help').catch(() => {});
