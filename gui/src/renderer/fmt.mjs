// Small formatting helpers shared across components.

export function fmtSecs(s) {
  const v = Math.max(0, Math.round(Number(s) || 0));
  const m = Math.floor(v / 60), r = v % 60;
  return `${m}:${r.toString().padStart(2, '0')}`;
}

export function fmtBigSecs(s) {
  const v = Math.max(0, Math.round(Number(s) || 0));
  const h = Math.floor(v / 3600);
  const m = Math.floor((v % 3600) / 60);
  const sec = v % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${sec}s`;
  return `${sec}s`;
}

export function escapeHtml(s) {
  return String(s == null ? '' : s).replace(/[<>&"]/g,
    c => ({ '<': '&lt;', '>': '&gt;', '&': '&amp;', '"': '&quot;' }[c]));
}
