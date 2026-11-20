//! Direct-audio-URL support for `/init`.
//!
//! A plain `http(s)://…/song.mp3` link is not a video page, so it is
//! downloaded directly instead of through yt-dlp. Before anything is written
//! to the library the URL is probed (Content-Type / extension) and the
//! downloaded bytes are validated with lofty, so an HTML error page can never
//! be registered as a song.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

/// Supported containers. Kept in sync with the local-import list, since both
/// paths feed the same rodio-based player.
pub const AUDIO_EXTS: [&str; 5] = ["mp3", "flac", "ogg", "oga", "wav"];
/// Hard cap for a single direct download (500 MiB).
pub const MAX_AUDIO_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct RemoteAudio {
    pub url: String,
    pub host: Option<String>,
    /// Decoded final path segment, e.g. `song.mp3`.
    pub filename: Option<String>,
    pub extension: String,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
}

pub fn is_http_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://")
}

pub fn is_supported_extension(ext: &str) -> bool {
    AUDIO_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Lowercase extension from the URL path, if it is a supported audio one.
pub fn audio_extension_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.last()?;
    let (_, ext) = last.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    if is_supported_extension(&ext) {
        Some(ext)
    } else {
        None
    }
}

/// Map an audio Content-Type to the container extension.
pub fn extension_from_content_type(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" => Some("mp3"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "audio/ogg" | "application/ogg" | "audio/vorbis" => Some("ogg"),
        "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/vnd.wave" => Some("wav"),
        _ => None,
    }
}

fn definitely_not_audio(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.contains("html")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.starts_with("image/")
        || ct.starts_with("video/")
}

fn decode_segment(segment: &str) -> String {
    percent_encoding::percent_decode_str(segment)
        .decode_utf8_lossy()
        .into_owned()
}

/// Human-friendly, filesystem-safe base name for a direct URL. The library's
/// own path sanitiser still runs later; this only strips the extension/query.
pub fn derive_name_from_url(url: &str) -> String {
    let path = reqwest::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| {
            url.split(['?', '#'])
                .next()
                .unwrap_or(url)
                .to_string()
        });
    let last = path
        .rsplit('/')
        .find(|s| !s.trim().is_empty())
        .unwrap_or("");
    let decoded = decode_segment(last);
    let stem = decoded
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(decoded.as_str())
        .trim()
        .trim_matches('.');
    if stem.is_empty() {
        "download".to_string()
    } else {
        stem.chars().take(120).collect()
    }
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let last = parsed
        .path_segments()?
        .filter(|s| !s.trim().is_empty())
        .last()?;
    let name = decode_segment(last);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

pub fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .user_agent("sjnmusicd/0.1 (https://github.com/sjnmusic)")
        .build()
        .map_err(|e| format!("HTTP client: {e}"))
}

fn header_value(resp: &reqwest::blocking::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Probe a URL without downloading the file. HEAD is tried first; servers
/// that reject HEAD (403/405/501) get a one-byte ranged GET instead.
fn probe_headers(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<(Option<String>, Option<u64>), String> {
    let head = client.head(url).send();
    let resp = match head {
        Ok(resp) if resp.status().is_success() => Some(resp),
        Ok(resp) => {
            let status = resp.status();
            if status == reqwest::StatusCode::FORBIDDEN
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
                || status == reqwest::StatusCode::NOT_IMPLEMENTED
            {
                let ranged = client
                    .get(url)
                    .header(reqwest::header::RANGE, "bytes=0-0")
                    .send()
                    .map_err(|e| format!("URL unreachable: {e}"))?;
                if ranged.status().is_success()
                    || ranged.status() == reqwest::StatusCode::PARTIAL_CONTENT
                {
                    Some(ranged)
                } else {
                    return Err(format!("URL returned HTTP {}", ranged.status()));
                }
            } else {
                return Err(format!("URL returned HTTP {status}"));
            }
        }
        Err(e) => return Err(format!("URL unreachable: {e}")),
    };
    let resp = resp.ok_or("URL probe failed")?;
    let content_type = header_value(&resp, "content-type");
    let content_length = header_value(&resp, "content-length").and_then(|v| v.parse().ok());
    Ok((content_type, content_length))
}

/// Validate that a URL points at a supported audio file. Returns the
/// container extension inferred from Content-Type or the URL path.
pub fn probe_audio_url(url: &str) -> Result<RemoteAudio, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "unsupported URL scheme {:?} (only http/https)",
            parsed.scheme()
        ));
    }
    let client = http_client()?;
    let (content_type, content_length) = probe_headers(&client, url)?;
    if let Some(ct) = content_type.as_deref() {
        if definitely_not_audio(ct) {
            return Err(format!("URL is not an audio file (Content-Type: {ct})"));
        }
    }
    let ext = content_type
        .as_deref()
        .and_then(extension_from_content_type)
        .map(str::to_string)
        .or_else(|| audio_extension_from_url(url))
        .ok_or_else(|| {
            format!(
                "URL does not point to a supported audio file (supported: {})",
                AUDIO_EXTS.join(", ")
            )
        })?;
    let host = parsed.host_str().map(|h| h.to_string());
    Ok(RemoteAudio {
        url: url.to_string(),
        host,
        filename: filename_from_url(url),
        extension: ext,
        content_type,
        content_length,
    })
}

/// Download `url` into `dest`, enforcing the size cap. The destination is
/// created/truncated; callers download into a temp file inside the library.
pub fn download_to(url: &str, dest: &Path) -> Result<u64, String> {
    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download returned HTTP {}", resp.status()));
    }
    if let Some(ct) = header_value(&resp, "content-type") {
        if definitely_not_audio(&ct) {
            return Err(format!("URL is not an audio file (Content-Type: {ct})"));
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create library dir: {e}"))?;
    }
    let mut file = std::fs::File::create(dest).map_err(|e| format!("create temp file: {e}"))?;
    let mut limited = resp.take(MAX_AUDIO_BYTES + 1);
    let bytes = std::io::copy(&mut limited, &mut file).map_err(|e| format!("download: {e}"))?;
    if bytes == 0 {
        let _ = std::fs::remove_file(dest);
        return Err("download was empty".to_string());
    }
    if bytes > MAX_AUDIO_BYTES {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "download exceeds the {} MiB limit",
            MAX_AUDIO_BYTES / (1024 * 1024)
        ));
    }
    file.sync_all().map_err(|e| format!("flush download: {e}"))?;
    Ok(bytes)
}

/// Reject anything that is not parseable audio. This is the "is it really a
/// song?" gate: an HTML error page, a JSON blob or a truncated file all fail
/// here even when the server advertised a misleading extension.
pub fn validate_audio_file(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("downloaded file missing: {e}"))?;
    if meta.len() < 128 {
        return Err("downloaded file is too small to be audio".to_string());
    }
    lofty::read_from_path(path)
        .map_err(|e| format!("downloaded file is not a valid audio file: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_http_urls_only() {
        assert!(is_http_url("https://serverjonas.de/songs/song.mp3"));
        assert!(is_http_url("http://example.com/a.flac"));
        assert!(!is_http_url("ytsearch1:song"));
        assert!(!is_http_url("/home/user/song.mp3"));
    }

    #[test]
    fn derives_names_from_file_urls() {
        assert_eq!(
            derive_name_from_url("https://serverjonas.de/songs/My%20Song.mp3?x=1"),
            "My Song"
        );
        assert_eq!(
            derive_name_from_url("http://example.com/a/b/track"),
            "track"
        );
        assert_eq!(derive_name_from_url("https://example.com/"), "download");
    }

    #[test]
    fn infers_extensions_from_url_and_content_type() {
        assert_eq!(
            audio_extension_from_url("https://serverjonas.de/songs/song.MP3"),
            Some("mp3".to_string())
        );
        assert_eq!(
            audio_extension_from_url("https://serverjonas.de/songs/song.mp4"),
            None
        );
        assert_eq!(extension_from_content_type("audio/mpeg"), Some("mp3"));
        assert_eq!(extension_from_content_type("audio/flac"), Some("flac"));
        assert_eq!(extension_from_content_type("text/html"), None);
    }

    #[test]
    fn html_is_rejected_by_validation() {
        let dir = std::env::temp_dir().join(format!(
            "sjnremote-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("page.mp3");
        std::fs::write(&path, b"<!doctype html><html><body>404</body></html>").unwrap();
        assert!(validate_audio_file(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
