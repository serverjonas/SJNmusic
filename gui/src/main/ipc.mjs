// IPC surface exposed to the renderer via preload. Each handler is a thin
// wrapper around the DaemonClient, so the renderer never holds a raw socket.

export function installHandlers(ipcMain, daemon) {
  const wrap = (method) => async (_e, { path, body, query } = {}) => {
    try {
      return { ok: true, data: await daemon[method](path, query ?? body) };
    } catch (err) {
      const status = err.status || 0;
      const causedByFetch = !!err.causedByFetch;
      const msg = err.daemonResponse?.error || err.message;
      return { ok: false, error: msg, status, causedByFetch };
    }
  };

  ipcMain.handle('daemon:get',   wrap('get'));
  ipcMain.handle('daemon:post',  wrap('post'));
  ipcMain.handle('daemon:patch', wrap('patch'));
  ipcMain.handle('daemon:put',   wrap('put'));
  ipcMain.handle('daemon:del',   wrap('del'));
}
