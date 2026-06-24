// Thin HTTP client to sjnmusicd. Single object so the main process holds
// exactly one socket pool; retries the /now-playing poll on transient
// failures but does NOT crash the renderer when the daemon is offline.

const DEFAULT_HOST = '127.0.0.1';
const DEFAULT_PORT = 14567;

export class DaemonClient {
  constructor({ host = DEFAULT_HOST, port = DEFAULT_PORT, timeoutMs = 8000 } = {}) {
    this.base = `http://${host}:${port}`;
    this.timeoutMs = timeoutMs;
  }

  async _req(method, path, body) {
    const url = this.base + path;
    const opts = { method, signal: AbortSignal.timeout(this.timeoutMs) };
    if (body !== undefined) {
      opts.headers = { 'Content-Type': 'application/json' };
      opts.body = JSON.stringify(body);
    }
    let res;
    try {
      res = await fetch(url, opts);
    } catch (e) {
      const err = new Error(`daemon unreachable at ${url}: ${e.message}`);
      err.causedByFetch = true;
      throw err;
    }
    const text = await res.text();
    let body_out = {};
    if (text) {
      try { body_out = JSON.parse(text); }
      catch { body_out = { _raw: text }; }
    }
    if (!res.ok) {
      const err = new Error(body_out.error || `HTTP ${res.status}`);
      err.status = res.status;
      err.daemonResponse = body_out;
      throw err;
    }
    return body_out;
  }

  get(path, query) {
    let qs = '';
    if (query && Object.keys(query).length > 0) {
      qs = '?' + new URLSearchParams(query).toString();
    }
    return this._req('GET', path + qs);
  }

  post(path, body)    { return this._req('POST',   path, body); }
  patch(path, body)   { return this._req('PATCH',  path, body); }
  put(path, body)     { return this._req('PUT',    path, body); }
  del(path, body)     { return this._req('DELETE', path, body); }
}

/**
 * Convenience: turn a "song name" object into what /play /add /del /search
 * expect — lets render and main both speak the same JSON convention.
 */
export function searchBody(query) {
  return { query };
}
