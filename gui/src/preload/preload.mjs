// preload: exposes a *narrow* IPC surface to the renderer.
// The renderer never sees Node, fs, or any daemon URL.
import { contextBridge, ipcRenderer } from 'electron';

// We accept both shapes from the renderer:
//   (a) flat options: window.api.get('/foo', { q: 'x' })
//   (b) wrapped:       window.api.post('/foo', { body: {...} })
//                       window.api.get('/foo', { query: {...} })
// Whichever is provided wins; otherwise the second arg is routed as body
// (for POST/PATCH/PUT) or query (for GET/DELETE).
function _invoke(channel, kind) {
  return async (path, opts = {}) => {
    const body  = kind === 'body'  ? (opts.body  ?? opts) : undefined;
    const query = kind === 'query' ? (opts.query ?? opts) : undefined;
    return ipcRenderer.invoke(channel, { path, body, query });
  };
}

const tickListeners = new Set();
ipcRenderer.on('tick', (_ev, data) => {
  for (const cb of tickListeners) {
    try { cb(data); } catch { /* never let one bad subscriber kill the bus */ }
  }
});

contextBridge.exposeInMainWorld('api', {
  get:   _invoke('daemon:get',   'query'),
  post:  _invoke('daemon:post',  'body'),
  patch: _invoke('daemon:patch', 'body'),
  put:   _invoke('daemon:put',   'body'),
  del:   _invoke('daemon:del',   'query'),

  onTick(callback) {
    tickListeners.add(callback);
    return () => tickListeners.delete(callback);
  },
});
