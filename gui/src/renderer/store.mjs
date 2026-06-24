// Shared store: one mutable object + a subscribers list. The renderer
// registers components, and the polling loop calls `store.set(...)` which
// triggers a render for each subscriber.

const state = {
  online: false,
  nowPlaying: null,
  queue: null,
  downloads: null,
  error: null,
  route: '#library',
  theme: localStorage.getItem('sjnmusic.theme') || 'dark',
  toasts: [],
};

const subs = new Set();

export const store = {
  state,
  set(partial) {
    Object.assign(state, partial);
    for (const cb of subs) {
      try { cb(state); } catch { /* defensive */ }
    }
  },
  subscribe(cb) {
    subs.add(cb);
    cb(state);
    return () => subs.delete(cb);
  },
};

export function pushToast(message, level = 'info', ttlMs = 3500) {
  const id = Math.random().toString(36).slice(2);
  state.toasts.push({ id, message, level });
  store.set({ toasts: [...state.toasts] });
  if (ttlMs > 0) {
    setTimeout(() => {
      state.toasts = state.toasts.filter(t => t.id !== id);
      store.set({ toasts: [...state.toasts] });
    }, ttlMs);
  }
}
