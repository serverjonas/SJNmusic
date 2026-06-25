//! Small formatting helpers used across views.

/// Format seconds as `m:ss` (the transport footer / row meta label).
pub fn mmss(secs: f64) -> String {
    let v = secs.max(0.0).round() as i64;
    let m = v / 60;
    let s = v % 60;
    format!("{}:{:02}", m, s)
}

/// Format seconds as a human duration, used by Stats cards.
pub fn big_secs(secs: f64) -> String {
    let v = secs.max(0.0).round() as i64;
    let h = v / 3600;
    let m = (v % 3600) / 60;
    let s = v % 60;
    if h > 0 {
        return format!("{}h {}m", h, m);
    }
    if m > 0 {
        return format!("{}m {}s", m, s);
    }
    format!("{}s", s)
}

/// HTML-escape for `egui::RichText` doesn't need it but for raw markup
/// we still want a helper to centralise the rules.
#[allow(dead_code)]
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}
