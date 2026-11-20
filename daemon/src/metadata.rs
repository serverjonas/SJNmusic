//! Automatic music-metadata recognition.
//!
//! Pipeline: file tags -> Chromaprint fingerprint (`fpcalc`) -> AcoustID
//! lookup -> MusicBrainz recording/release enrichment -> merged DB row ->
//! tags written back into the audio file.
//!
//! Design notes
//! - All network work happens on background threads. The HTTP handlers only
//!   queue work, so an unreachable AcoustID/MusicBrainz never blocks the
//!   daemon or the playback loop.
//! - Rate limits are process-global (`Mutex<Instant>` per service), so
//!   parallel scans cannot exceed them.
//! - Lookup responses are cached in SQLite (`metadata_cache`); generated
//!   fingerprints are cached on the song row and reused.
//! - A failed lookup never clears existing metadata. Only a successful,
//!   unambiguous match writes the metadata columns; the original
//!   `source_*` columns are never touched by this module.
//! - Tag writes are atomic: the file is copied to a sibling temp file,
//!   rewritten there, then renamed over the original.

use std::path::Path;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, info, warn};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{MetadataConfig, MultiMatchPolicy};
use crate::paths::db_path;
use crate::state::{song_from_row, Song};

// ---------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------

static CONFIG: RwLock<Option<MetadataConfig>> = RwLock::new(None);

pub fn initialize(cfg: &MetadataConfig) {
    if let Ok(mut guard) = CONFIG.write() {
        *guard = Some(cfg.clone());
    }
}

pub fn set_config(cfg: &MetadataConfig) {
    initialize(cfg);
}

pub fn current_config() -> MetadataConfig {
    CONFIG
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

/// True when `fpcalc` (Chromaprint) is available on PATH.
pub fn fpcalc_available() -> bool {
    std::process::Command::new("fpcalc")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// Rate limiting + HTTP
// ---------------------------------------------------------------------

static MB_LAST: Mutex<Option<Instant>> = Mutex::new(None);
static ACOUSTID_LAST: Mutex<Option<Instant>> = Mutex::new(None);

/// Bounds recognition concurrency to one pipeline at a time. Scans are
/// already sequential; this keeps a large batched import from spawning an
/// unbounded number of concurrent fingerprint/HTTP workers. The service
/// rate limits are global regardless, so throughput is unchanged.
static RECOGNITION_GATE: Mutex<()> = Mutex::new(());

fn throttle(service: Service) {
    let (lock, min_interval) = match service {
        // MusicBrainz asks for at most one request per second.
        Service::MusicBrainz => (&MB_LAST, Duration::from_millis(1100)),
        Service::AcoustId => (&ACOUSTID_LAST, Duration::from_millis(400)),
    };
    let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(last) = *guard {
        let elapsed = last.elapsed();
        if elapsed < min_interval {
            std::thread::sleep(min_interval - elapsed);
        }
    }
    *guard = Some(Instant::now());
}

#[derive(Clone, Copy)]
enum Service {
    MusicBrainz,
    AcoustId,
}

fn http_client(cfg: &MetadataConfig) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(25))
        .user_agent(cfg.user_agent.clone())
        .build()
        .map_err(|e| format!("HTTP client: {e}"))
}

// ---------------------------------------------------------------------
// SQLite helpers (own connection: never holds the daemon state mutex
// across network I/O)
// ---------------------------------------------------------------------

pub(crate) fn metadata_conn() -> Result<Connection, String> {
    let conn = Connection::open(db_path()).map_err(|e| format!("DB open: {e}"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| format!("DB busy_timeout: {e}"))?;
    let _ = conn.execute_batch("PRAGMA foreign_keys = ON;");
    Ok(conn)
}

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cache_key(kind: &str, input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{kind}:{:x}", hasher.finalize())
}

fn cache_get(conn: &Connection, key: &str, max_age_secs: u64) -> Option<Value> {
    if max_age_secs == 0 {
        return None;
    }
    let (payload, fetched_at): (String, i64) = conn
        .query_row(
            "SELECT payload, fetched_at FROM metadata_cache WHERE cache_key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()?;
    let age = unix_secs() - fetched_at;
    if age < 0 || age as u64 > max_age_secs {
        return None;
    }
    serde_json::from_str(&payload).ok()
}

fn cache_put(conn: &Connection, key: &str, value: &Value) {
    let payload = value.to_string();
    if let Err(e) = conn.execute(
        "INSERT INTO metadata_cache (cache_key, payload, fetched_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(cache_key) DO UPDATE SET
             payload = excluded.payload,
             fetched_at = excluded.fetched_at",
        params![key, payload, unix_secs()],
    ) {
        debug!("metadata cache write failed: {e}");
    }
}

// ---------------------------------------------------------------------
// Track metadata model
// ---------------------------------------------------------------------

/// Every field a recognition run can produce, in DB column order. `None`
/// means "not available" and is never invented.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub songwriter: Option<String>,
    pub arranger: Option<String>,
    pub performer: Option<String>,
    pub release_date: Option<String>,
    pub original_release_date: Option<String>,
    pub release_year: Option<i64>,
    pub track_number: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_number: Option<i64>,
    pub disc_total: Option<i64>,
    pub genre: Option<String>,
    pub tags: Option<String>,
    pub style: Option<String>,
    pub recording_location: Option<String>,
    pub language: Option<String>,
    pub isrc: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub barcode: Option<String>,
    pub release_type: Option<String>,
    pub release_country: Option<String>,
    pub bpm: Option<f64>,
    pub musical_key: Option<String>,
    pub musicbrainz_recording_id: Option<String>,
    pub musicbrainz_release_id: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
    pub musicbrainz_release_artist_id: Option<String>,
    pub musicbrainz_work_id: Option<String>,
    pub musicbrainz_track_id: Option<String>,
    pub acoustid_id: Option<String>,
}

macro_rules! fill_missing {
    ($dst:expr, $src:expr; $($field:ident),+ $(,)?) => {
        $(
            if $dst.$field.is_none() {
                $dst.$field = $src.$field.clone();
            }
        )+
    };
}

impl TrackTags {
    /// Fall back field-by-field to `other`. Never overwrites a value we
    /// already have with `None`.
    fn fill_from(&mut self, other: &TrackTags) {
        fill_missing!(self, other;
            title, artist, album, album_artist, composer, songwriter, arranger,
            performer, release_date, original_release_date, release_year,
            track_number, track_total, disc_number, disc_total, genre, tags,
            style, recording_location, language, isrc, label, catalog_number,
            barcode, release_type, release_country, bpm, musical_key,
            musicbrainz_recording_id, musicbrainz_release_id,
            musicbrainz_release_group_id, musicbrainz_artist_id,
            musicbrainz_release_artist_id, musicbrainz_work_id,
            musicbrainz_track_id, acoustid_id,
        );
    }
}

fn tags_from_song(song: &Song) -> TrackTags {
    TrackTags {
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
        album_artist: song.album_artist.clone(),
        composer: song.composer.clone(),
        songwriter: song.songwriter.clone(),
        arranger: song.arranger.clone(),
        performer: song.performer.clone(),
        release_date: song.release_date.clone(),
        original_release_date: song.original_release_date.clone(),
        release_year: song.release_year,
        track_number: song.track_number,
        track_total: song.track_total,
        disc_number: song.disc_number,
        disc_total: song.disc_total,
        genre: song.genre.clone(),
        tags: song.tags.clone(),
        style: song.style.clone(),
        recording_location: song.recording_location.clone(),
        language: song.language.clone(),
        isrc: song.isrc.clone(),
        label: song.label.clone(),
        catalog_number: song.catalog_number.clone(),
        barcode: song.barcode.clone(),
        release_type: song.release_type.clone(),
        release_country: song.release_country.clone(),
        bpm: song.bpm,
        musical_key: song.musical_key.clone(),
        musicbrainz_recording_id: song.musicbrainz_recording_id.clone(),
        musicbrainz_release_id: song.musicbrainz_release_id.clone(),
        musicbrainz_release_group_id: song.musicbrainz_release_group_id.clone(),
        musicbrainz_artist_id: song.musicbrainz_artist_id.clone(),
        musicbrainz_release_artist_id: song.musicbrainz_release_artist_id.clone(),
        musicbrainz_work_id: song.musicbrainz_work_id.clone(),
        musicbrainz_track_id: song.musicbrainz_track_id.clone(),
        acoustid_id: song.acoustid_id.clone(),
    }
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn json_str<'a>(v: &'a Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    clean(cur.as_str().map(str::to_string))
}

// ---------------------------------------------------------------------
// File tag reading (lofty)
// ---------------------------------------------------------------------

fn tag_string(tag: &lofty::tag::Tag, key: lofty::tag::ItemKey) -> Option<String> {
    clean(tag.get_string(&key).map(str::to_string))
}

/// Read existing tags from an audio file. Errors are non-fatal: a
/// recognition run can still fingerprint a file whose tags are corrupt.
pub fn read_file_tags(path: &str) -> Result<TrackTags, String> {
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey};

    let tagged = lofty::read_from_path(path).map_err(|e| format!("read tags: {e}"))?;
    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return Ok(TrackTags::default()),
    };
    let bpm = tag_string(tag, ItemKey::IntegerBpm)
        .or_else(|| tag_string(tag, ItemKey::Bpm))
        .and_then(|s| s.trim().parse::<f64>().ok());
    Ok(TrackTags {
        title: clean(tag.title().map(|s| s.to_string())),
        artist: clean(tag.artist().map(|s| s.to_string())),
        album: clean(tag.album().map(|s| s.to_string())),
        album_artist: tag_string(tag, ItemKey::AlbumArtist),
        composer: tag_string(tag, ItemKey::Composer),
        songwriter: tag_string(tag, ItemKey::Writer)
            .or_else(|| tag_string(tag, ItemKey::Lyricist)),
        arranger: tag_string(tag, ItemKey::Arranger),
        performer: tag_string(tag, ItemKey::Performer),
        release_date: tag_string(tag, ItemKey::RecordingDate)
            .or_else(|| tag_string(tag, ItemKey::ReleaseDate)),
        original_release_date: tag_string(tag, ItemKey::OriginalReleaseDate),
        release_year: tag.year().map(|y| y as i64),
        track_number: tag.track().map(|v| v as i64),
        track_total: tag.track_total().map(|v| v as i64),
        disc_number: tag.disk().map(|v| v as i64),
        disc_total: tag.disk_total().map(|v| v as i64),
        genre: clean(tag.genre().map(|s| s.to_string())),
        tags: None,
        style: None,
        recording_location: None,
        language: tag_string(tag, ItemKey::Language),
        isrc: tag_string(tag, ItemKey::Isrc),
        label: tag_string(tag, ItemKey::Label),
        catalog_number: tag_string(tag, ItemKey::CatalogNumber),
        barcode: tag_string(tag, ItemKey::Barcode),
        release_type: None,
        release_country: None,
        bpm,
        musical_key: tag_string(tag, ItemKey::InitialKey),
        musicbrainz_recording_id: tag_string(tag, ItemKey::MusicBrainzRecordingId)
            .or_else(|| {
                tag_string(
                    tag,
                    ItemKey::Unknown("MusicBrainz Recording Id".to_string()),
                )
            }),
        musicbrainz_release_id: tag_string(tag, ItemKey::MusicBrainzReleaseId),
        musicbrainz_release_group_id: tag_string(tag, ItemKey::MusicBrainzReleaseGroupId),
        musicbrainz_artist_id: tag_string(tag, ItemKey::MusicBrainzArtistId),
        musicbrainz_release_artist_id: tag_string(tag, ItemKey::MusicBrainzReleaseArtistId),
        musicbrainz_work_id: tag_string(tag, ItemKey::MusicBrainzWorkId),
        musicbrainz_track_id: tag_string(tag, ItemKey::MusicBrainzTrackId),
        // Lofty 0.22 has no first-class AcoustID key; the common
        // `ACOUSTID_ID` Vorbis comment is read through the unknown-key
        // path where the format supports it.
        acoustid_id: tag
            .get_string(&ItemKey::Unknown("ACOUSTID_ID".to_string()))
            .map(str::to_string)
            .and_then(|s| clean(Some(s))),
    })
}

// ---------------------------------------------------------------------
// Chromaprint fingerprint
// ---------------------------------------------------------------------

fn encode_fingerprint(duration: f64, fp: &str) -> String {
    format!("{duration}\n{fp}")
}

fn decode_fingerprint(raw: &str) -> Option<(f64, String)> {
    let (d, f) = raw.split_once('\n')?;
    let duration = d.trim().parse::<f64>().ok()?;
    let fp = f.trim();
    if fp.is_empty() {
        None
    } else {
        Some((duration, fp.to_string()))
    }
}

/// Run `fpcalc` for `path`. Returns `(duration_seconds, fingerprint)`.
pub fn fingerprint_file(path: &str) -> Result<(f64, String), String> {
    let output = std::process::Command::new("fpcalc")
        .args(["-json", path])
        .output()
        .map_err(|e| format!("fpcalc unavailable: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("fpcalc failed: {}", stderr.trim()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        let duration = v.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0);
        if let Some(fp) = v.get("fingerprint").and_then(|f| f.as_str()) {
            if !fp.is_empty() {
                return Ok((duration, fp.to_string()));
            }
        }
    }
    // Older fpcalc builds without `-json` support print KEY=VALUE lines.
    let mut duration = 0.0;
    let mut fingerprint = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("DURATION=") {
            duration = rest.trim().parse().unwrap_or(0.0);
        } else if let Some(rest) = line.strip_prefix("FINGERPRINT=") {
            fingerprint = rest.trim().to_string();
        }
    }
    if fingerprint.is_empty() {
        Err("fpcalc produced no fingerprint".to_string())
    } else {
        Ok((duration, fingerprint))
    }
}

// ---------------------------------------------------------------------
// AcoustID
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
struct AcoustIdCandidate {
    acoustid_id: String,
    score: f64,
    recording_id: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    release_group_id: Option<String>,
    release_group_type: Option<String>,
}

fn parse_acoustid(v: &Value) -> Vec<AcoustIdCandidate> {
    let mut out = Vec::new();
    let Some(results) = v.get("results").and_then(|r| r.as_array()) else {
        return out;
    };
    for result in results {
        let acoustid_id = json_str(result, &["id"]).unwrap_or_default();
        let score = result.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let Some(recordings) = result.get("recordings").and_then(|r| r.as_array()) else {
            if !acoustid_id.is_empty() {
                out.push(AcoustIdCandidate {
                    acoustid_id,
                    score,
                    recording_id: None,
                    title: None,
                    artist: None,
                    release_group_id: None,
                    release_group_type: None,
                });
            }
            continue;
        };
        for rec in recordings {
            let artist = rec
                .get("artists")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .and_then(|s| clean(Some(s)));
            let rg = rec
                .get("releasegroups")
                .and_then(|r| r.as_array())
                .and_then(|a| a.first());
            out.push(AcoustIdCandidate {
                acoustid_id: acoustid_id.clone(),
                score,
                recording_id: json_str(rec, &["id"]),
                title: json_str(rec, &["title"]),
                artist,
                release_group_id: rg.and_then(|r| json_str(r, &["id"])),
                release_group_type: rg.and_then(|r| json_str(r, &["type"])),
            });
        }
    }
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn acoustid_lookup(
    cfg: &MetadataConfig,
    conn: &Connection,
    duration: f64,
    fingerprint: &str,
    use_cache: bool,
) -> Result<Vec<AcoustIdCandidate>, String> {
    if !cfg.acoustid_configured() {
        return Err("AcoustID API key not configured".to_string());
    }
    let client = http_client(cfg)?;
    let key = cache_key(
        "acoustid",
        &format!("{}|{}|{}", cfg.acoustid_api_key, duration, fingerprint),
    );
    if use_cache {
        if let Some(v) = cache_get(conn, &key, cfg.cache_days.saturating_mul(86_400)) {
            return Ok(parse_acoustid(&v));
        }
    }
    throttle(Service::AcoustId);
    // POST keeps long fingerprints out of the URL (some fingerprints are
    // several kilobytes); AcoustID accepts the same fields as form data.
    let duration_str = format!("{duration:.0}");
    let resp = client
        .post(&cfg.acoustid_endpoint)
        .form(&[
            ("client", cfg.acoustid_api_key.as_str()),
            ("meta", "recordings+releasegroups"),
            ("duration", duration_str.as_str()),
            ("fingerprint", fingerprint),
        ])
        .send()
        .map_err(|e| format!("AcoustID request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| format!("AcoustID response: {e}"))?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("AcoustID returned invalid JSON (HTTP {status}): {e}"))?;
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            return Err(format!("AcoustID error: {}", err));
        }
    }
    let api_status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if api_status != "ok" {
        return Err(format!(
            "AcoustID status {:?}: {}",
            api_status,
            v.get("error").cloned().unwrap_or(Value::Null)
        ));
    }
    cache_put(conn, &key, &v);
    Ok(parse_acoustid(&v))
}

// ---------------------------------------------------------------------
// MusicBrainz
// ---------------------------------------------------------------------

fn mb_get(
    cfg: &MetadataConfig,
    conn: &Connection,
    recording_id: &str,
    use_cache: bool,
) -> Result<Value, String> {
    let client = http_client(cfg)?;
    let key = cache_key("musicbrainz", recording_id);
    if use_cache {
        if let Some(v) = cache_get(conn, &key, cfg.cache_days.saturating_mul(86_400)) {
            return Ok(v);
        }
    }
    let base = cfg.musicbrainz_endpoint.trim_end_matches('/');
    let url = format!("{base}/recording/{recording_id}");
    throttle(Service::MusicBrainz);
    let resp = client
        .get(&url)
        .query(&[
            (
                "inc",
                "artists+releases+release-groups+isrcs+tags+genres+work-rels+recording-rels",
            ),
            ("fmt", "json"),
        ])
        .send()
        .map_err(|e| format!("MusicBrainz request failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("MusicBrainz recording not found: {recording_id}"));
    }
    if !status.is_success() {
        return Err(format!("MusicBrainz returned HTTP {status}"));
    }
    let v: Value = resp
        .json()
        .map_err(|e| format!("MusicBrainz returned invalid JSON: {e}"))?;
    cache_put(conn, &key, &v);
    Ok(v)
}

fn artist_credit_names(release: &Value) -> Option<String> {
    let arr = release.get("artist-credit")?.as_array()?;
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn artist_credit_id(release: &Value) -> Option<String> {
    release
        .get("artist-credit")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("artist"))
        .and_then(|a| a.get("id"))
        .and_then(|i| i.as_str())
        .map(str::to_string)
}

/// Pick the most useful release for album-level fields. Album releases
/// with a date/label win over singles, but any release is better than none.
fn pick_release(recording: &Value) -> Option<&Value> {
    let releases = recording.get("releases")?.as_array()?;
    releases.iter().max_by_key(|r| {
        let primary = r
            .get("release-group")
            .and_then(|g| g.get("primary-type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let status = r.get("status").and_then(|s| s.as_str()).unwrap_or("");
        let mut score = 0i32;
        if primary.eq_ignore_ascii_case("album") {
            score += 4;
        } else if primary.eq_ignore_ascii_case("ep") {
            score += 2;
        }
        if status.eq_ignore_ascii_case("official") {
            score += 2;
        }
        if r.get("date").and_then(|d| d.as_str()).is_some() {
            score += 1;
        }
        if r.get("label-info").and_then(|l| l.as_array()).is_some() {
            score += 1;
        }
        score
    })
}

fn collect_tags(recording: &Value) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    for field in ["tags", "genres"] {
        if let Some(arr) = recording.get(field).and_then(|t| t.as_array()) {
            for item in arr {
                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                    if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn tags_from_mb(v: &Value, recording_id: &str) -> TrackTags {
    let mut t = TrackTags {
        musicbrainz_recording_id: Some(recording_id.to_string()),
        title: json_str(v, &["title"]),
        artist: artist_credit_names(v),
        musicbrainz_artist_id: artist_credit_id(v),
        isrc: v
            .get("isrcs")
            .and_then(|i| i.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.as_str())
            .map(str::to_string),
        tags: collect_tags(v),
        ..Default::default()
    };
    t.genre = t
        .tags
        .as_deref()
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Work relation (performance -> work) provides the MB work id.
    if let Some(relations) = v.get("relations").and_then(|r| r.as_array()) {
        for rel in relations {
            if let Some(work_id) = json_str(rel, &["work", "id"]) {
                t.musicbrainz_work_id = Some(work_id);
            }
            if rel.get("type").and_then(|x| x.as_str()) == Some("recorded at") {
                let place = json_str(rel, &["place", "name"])
                    .or_else(|| json_str(rel, &["area", "name"]));
                if place.is_some() {
                    t.recording_location = place;
                }
            }
        }
    }
    if t.language.is_none() {
        t.language = json_str(v, &["language"]);
    }

    if let Some(release) = pick_release(v) {
        t.album = json_str(release, &["title"]);
        t.musicbrainz_release_id = json_str(release, &["id"]);
        t.release_date = json_str(release, &["date"]);
        t.release_country = json_str(release, &["country"]);
        t.barcode = json_str(release, &["barcode"]);
        t.album_artist = artist_credit_names(release);
        t.musicbrainz_release_artist_id = artist_credit_id(release);
        t.musicbrainz_release_group_id = json_str(release, &["release-group", "id"]);
        t.original_release_date =
            json_str(release, &["release-group", "first-release-date"]);
        t.release_year = t
            .release_date
            .as_deref()
            .or(t.original_release_date.as_deref())
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse::<i64>().ok());

        let primary = json_str(release, &["release-group", "primary-type"]);
        let secondary: Vec<String> = release
            .get("release-group")
            .and_then(|g| g.get("secondary-types"))
            .and_then(|s| s.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !secondary.is_empty() {
            t.style = Some(secondary.join(", "));
        }
        t.release_type = match (primary, secondary.is_empty()) {
            (Some(p), true) => Some(p),
            (Some(p), false) => Some(format!("{p} + {}", secondary.join(", "))),
            (None, false) => Some(secondary.join(", ")),
            (None, true) => None,
        };

        if let Some(info) = release
            .get("label-info")
            .and_then(|l| l.as_array())
            .and_then(|a| a.first())
        {
            t.label = json_str(info, &["label", "name"]);
            t.catalog_number = json_str(info, &["catalog-number"]);
        }

        // Track/disc numbers from the medium that contains this recording.
        if let Some(media) = release.get("media").and_then(|m| m.as_array()) {
            t.disc_total = Some(media.len() as i64);
            'outer: for medium in media {
                t.disc_number = medium.get("position").and_then(|p| p.as_i64());
                t.track_total = medium.get("track-count").and_then(|p| p.as_i64());
                if let Some(tracks) = medium.get("tracks").and_then(|x| x.as_array()) {
                    for track in tracks {
                        let matches_recording = track
                            .get("recording")
                            .and_then(|r| r.get("id"))
                            .and_then(|i| i.as_str())
                            .map(|id| id == recording_id)
                            .unwrap_or(false);
                        if matches_recording {
                            t.track_number = track.get("position").and_then(|p| p.as_i64());
                            t.musicbrainz_track_id = json_str(track, &["id"]);
                            break 'outer;
                        }
                    }
                }
            }
            if t.track_number.is_none() {
                if let Some(track) = media
                    .first()
                    .and_then(|m| m.get("tracks"))
                    .and_then(|t| t.as_array())
                    .and_then(|a| a.first())
                {
                    t.track_number = track.get("position").and_then(|p| p.as_i64());
                    t.musicbrainz_track_id = json_str(track, &["id"]);
                }
            }
        }
    }
    t
}

// ---------------------------------------------------------------------
// Recognition outcome + orchestration
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RecognizeOutcome {
    pub song_id: i64,
    pub song_name: String,
    /// recognized | partial | ambiguous | not_found | error | skipped
    pub status: String,
    pub source: Option<String>,
    pub confidence: Option<f64>,
    pub error: Option<String>,
    pub database_updated: bool,
    pub file_updated: bool,
    pub file_error: Option<String>,
}

impl RecognizeOutcome {
    /// True for any outcome that produced usable metadata.
    pub fn is_recognized(&self) -> bool {
        self.status == "recognized" || self.status == "partial"
    }
}

/// Recognize exactly one song by DB id. Never returns `Err` for lookup
/// failures — those land in `outcome.status == "error"` and leave the
/// existing database values untouched.
pub fn recognize_song(song_id: i64, force: bool) -> Result<RecognizeOutcome, String> {
    let _gate = RECOGNITION_GATE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let cfg = current_config();
    let conn = metadata_conn()?;
    let song: Song = conn
        .query_row(
            "SELECT * FROM songs WHERE id = ?1",
            params![song_id],
            song_from_row,
        )
        .optional()
        .map_err(|e| format!("DB read: {e}"))?
        .ok_or_else(|| format!("song {song_id} not found"))?;

    let mut outcome = RecognizeOutcome {
        song_id,
        song_name: song.name.clone(),
        status: "skipped".to_string(),
        ..Default::default()
    };
    if !force && song.is_metadata_complete() {
        return Ok(outcome);
    }

    if !Path::new(&song.path).exists() {
        outcome.status = "error".to_string();
        outcome.error = Some("file missing or moved".to_string());
        mark_checked(&conn, song_id, "error", outcome.error.as_deref());
        return Ok(outcome);
    }

    let file_tags = match read_file_tags(&song.path) {
        Ok(t) => t,
        Err(e) => {
            debug!("tag read failed for {}: {e}", song.path);
            TrackTags::default()
        }
    };

    let mut detected = TrackTags::default();
    let mut source: Option<String> = None;
    let mut confidence: Option<f64> = None;
    let mut acoustid_id: Option<String> = None;
    let mut acoustid_score: Option<f64> = None;
    let mut recording_id = song
        .musicbrainz_recording_id
        .clone()
        .or_else(|| file_tags.musicbrainz_recording_id.clone());
    let mut fingerprint: Option<String> = song.acoustid_fingerprint.clone();

    if !cfg.acoustid_configured() && recording_id.is_none() {
        outcome.status = "not_found".to_string();
        outcome.error = Some("AcoustID API key not configured".to_string());
        mark_checked(&conn, song_id, "not_found", outcome.error.as_deref());
        return Ok(outcome);
    }

    // A force scan re-queries AcoustID even when an existing recording id
    // is known, so changed/updated AcoustID data is picked up. Without
    // force (or without a key) the recording id is authoritative and the
    // fingerprint lookup is skipped entirely.
    let want_acoustid = recording_id.is_none() || (force && cfg.acoustid_configured());
    if want_acoustid {
        // Only spend time fingerprinting when AcoustID can actually be
        // asked, and only reuse a fingerprint we can decode.
        let parsed_fp = fingerprint
            .as_deref()
            .and_then(decode_fingerprint);
        let (duration, fp) = if let Some((d, f)) = parsed_fp {
            (d, f)
        } else {
            match fingerprint_file(&song.path) {
                Ok((d, f)) => {
                    fingerprint = Some(encode_fingerprint(d, &f));
                    (d, f)
                }
                Err(e) => {
                    outcome.status = "error".to_string();
                    outcome.error = Some(e.clone());
                    mark_checked(&conn, song_id, "error", Some(&e));
                    return Ok(outcome);
                }
            }
        };

        match acoustid_lookup(&cfg, &conn, duration, &fp, !force) {
            Ok(candidates) => {
                let policy = cfg.multi_match_policy();
                let min = cfg.min_confidence.clamp(0.0, 1.0);
                let filtered: Vec<&AcoustIdCandidate> =
                    candidates.iter().filter(|c| c.score >= min).collect();
                if filtered.is_empty() {
                    // Keep existing metadata/ids when the force lookup finds
                    // nothing; only a song with no id at all is "not found".
                    if recording_id.is_none() {
                        outcome.status = "not_found".to_string();
                        outcome.error =
                            Some("no AcoustID match above confidence".to_string());
                        outcome.confidence = candidates.first().map(|c| c.score);
                    }
                } else {
                    let top = filtered[0].score;
                    let distinct: std::collections::HashSet<String> = filtered
                        .iter()
                        .filter(|c| c.score >= top - 0.01)
                        .filter_map(|c| c.recording_id.clone())
                        .collect();
                    if distinct.len() > 1
                        && policy == MultiMatchPolicy::Skip
                        && recording_id.is_none()
                    {
                        outcome.status = "ambiguous".to_string();
                        outcome.error =
                            Some(format!("{} equally good matches", distinct.len()));
                        outcome.confidence = Some(top);
                    } else {
                        // Policy Best: prefer the richest candidate among
                        // the ties; otherwise the top one.
                        let chosen = filtered
                            .iter()
                            .take_while(|c| c.score >= top - 0.01)
                            .max_by_key(|c| {
                                c.release_group_id.is_some() as u8
                                    + c.title.is_some() as u8
                                    + c.artist.is_some() as u8
                            })
                            .copied()
                            .unwrap_or(filtered[0]);
                        acoustid_id = Some(chosen.acoustid_id.clone());
                        acoustid_score = Some(chosen.score);
                        confidence = Some(chosen.score);
                        recording_id = chosen.recording_id.clone();
                        detected.title = chosen.title.clone();
                        detected.artist = chosen.artist.clone();
                        detected.musicbrainz_release_group_id =
                            chosen.release_group_id.clone();
                        detected.style = chosen.release_group_type.clone();
                        source = Some("acoustid".to_string());
                    }
                }
            }
            Err(e) => {
                if let Some(fp) = fingerprint.as_deref() {
                    let _ = conn.execute(
                        "UPDATE songs SET acoustid_fingerprint = ?1 WHERE id = ?2",
                        params![fp, song_id],
                    );
                }
                // With a known recording id we still have a usable path
                // through MusicBrainz; only fail the song outright when
                // AcoustID was its sole chance of identification.
                if recording_id.is_none() {
                    outcome.status = "error".to_string();
                    outcome.error = Some(e.clone());
                    mark_checked(&conn, song_id, "error", Some(&e));
                    return Ok(outcome);
                }
                warn!("AcoustID lookup failed for {}: {e}", song.name);
            }
        }
    }
    if source.is_none() {
        if recording_id.is_some() {
            confidence = confidence.or(Some(song.acoustid_score.unwrap_or(1.0)));
            if acoustid_id.is_none() {
                acoustid_id = song.acoustid_id.clone().or(file_tags.acoustid_id.clone());
            }
            source = Some("tags+musicbrainz".to_string());
        }
    }

    // MusicBrainz enrichment (only with a recording id).
    let mut mb_error: Option<String> = None;
    if let Some(rid) = recording_id.clone() {
        if cfg.use_musicbrainz {
            match mb_get(&cfg, &conn, &rid, !force) {
                Ok(v) => {
                    let mb = tags_from_mb(&v, &rid);
                    detected.fill_from(&mb);
                    source = Some(match source.as_deref() {
                        Some("acoustid") => "acoustid+musicbrainz".to_string(),
                        _ => "musicbrainz".to_string(),
                    });
                    if confidence.is_none() {
                        confidence = Some(1.0);
                    }
                }
                Err(e) => {
                    warn!("MusicBrainz lookup failed for {rid}: {e}");
                    mb_error = Some(e);
                }
            }
        }
    }

    // Merge detected values with file tags and existing DB values. Nothing
    // is ever nulled out: missing fields keep what we already had.
    let mut merged = detected.clone();
    merged.fill_from(&file_tags);
    merged.fill_from(&tags_from_song(&song));
    if merged.musicbrainz_recording_id.is_none() {
        merged.musicbrainz_recording_id = recording_id.clone();
    }
    if merged.acoustid_id.is_none() {
        merged.acoustid_id = acoustid_id.clone();
    }

    let status = if recording_id.is_some() && mb_error.is_none() {
        "recognized"
    } else if acoustid_id.is_some() || recording_id.is_some() {
        // AcoustID matched but MusicBrainz enrichment is unavailable: keep
        // the usable data and let a later scan retry the enrichment.
        "partial"
    } else if source.is_some() {
        "recognized"
    } else if mb_error.is_some() {
        "error"
    } else {
        "not_found"
    };

    outcome.status = status.to_string();
    outcome.source = source.clone();
    outcome.confidence = confidence.or(acoustid_score);
    outcome.error = mb_error;

    // Persist. Only recognized/partial runs may rewrite the metadata
    // columns; error/ambiguous/not_found only update bookkeeping.
    let now = unix_secs();
    match status {
        "recognized" | "partial" => {
            if let Err(e) = conn.execute(
                "UPDATE songs SET
                    title = COALESCE(:title, title),
                    artist = COALESCE(:artist, artist),
                    album = COALESCE(:album, album),
                    album_artist = COALESCE(:album_artist, album_artist),
                    composer = COALESCE(:composer, composer),
                    songwriter = COALESCE(:songwriter, songwriter),
                    arranger = COALESCE(:arranger, arranger),
                    performer = COALESCE(:performer, performer),
                    release_date = COALESCE(:release_date, release_date),
                    original_release_date = COALESCE(:original_release_date, original_release_date),
                    release_year = COALESCE(:release_year, release_year),
                    track_number = COALESCE(:track_number, track_number),
                    track_total = COALESCE(:track_total, track_total),
                    disc_number = COALESCE(:disc_number, disc_number),
                    disc_total = COALESCE(:disc_total, disc_total),
                    genre = COALESCE(:genre, genre),
                    tags = COALESCE(:tags, tags),
                    style = COALESCE(:style, style),
                    recording_location = COALESCE(:recording_location, recording_location),
                    language = COALESCE(:language, language),
                    isrc = COALESCE(:isrc, isrc),
                    label = COALESCE(:label, label),
                    catalog_number = COALESCE(:catalog_number, catalog_number),
                    barcode = COALESCE(:barcode, barcode),
                    release_type = COALESCE(:release_type, release_type),
                    release_country = COALESCE(:release_country, release_country),
                    bpm = COALESCE(:bpm, bpm),
                    musical_key = COALESCE(:musical_key, musical_key),
                    musicbrainz_recording_id = COALESCE(:mb_recording, musicbrainz_recording_id),
                    musicbrainz_release_id = COALESCE(:mb_release, musicbrainz_release_id),
                    musicbrainz_release_group_id = COALESCE(:mb_release_group, musicbrainz_release_group_id),
                    musicbrainz_artist_id = COALESCE(:mb_artist, musicbrainz_artist_id),
                    musicbrainz_release_artist_id = COALESCE(:mb_release_artist, musicbrainz_release_artist_id),
                    musicbrainz_work_id = COALESCE(:mb_work, musicbrainz_work_id),
                    musicbrainz_track_id = COALESCE(:mb_track, musicbrainz_track_id),
                    acoustid_id = COALESCE(:acoustid_id, acoustid_id),
                    acoustid_score = COALESCE(:acoustid_score, acoustid_score),
                    acoustid_fingerprint = COALESCE(:fingerprint, acoustid_fingerprint),
                    acoustid_last_checked_at = :now,
                    metadata_source = :source,
                    metadata_confidence = :confidence,
                    metadata_last_updated_at = :now,
                    metadata_last_checked_at = :now,
                    metadata_status = :status,
                    metadata_error = :error",
                rusqlite::named_params! {
                    ":title": merged.title,
                    ":artist": merged.artist,
                    ":album": merged.album,
                    ":album_artist": merged.album_artist,
                    ":composer": merged.composer,
                    ":songwriter": merged.songwriter,
                    ":arranger": merged.arranger,
                    ":performer": merged.performer,
                    ":release_date": merged.release_date,
                    ":original_release_date": merged.original_release_date,
                    ":release_year": merged.release_year,
                    ":track_number": merged.track_number,
                    ":track_total": merged.track_total,
                    ":disc_number": merged.disc_number,
                    ":disc_total": merged.disc_total,
                    ":genre": merged.genre,
                    ":tags": merged.tags,
                    ":style": merged.style,
                    ":recording_location": merged.recording_location,
                    ":language": merged.language,
                    ":isrc": merged.isrc,
                    ":label": merged.label,
                    ":catalog_number": merged.catalog_number,
                    ":barcode": merged.barcode,
                    ":release_type": merged.release_type,
                    ":release_country": merged.release_country,
                    ":bpm": merged.bpm,
                    ":musical_key": merged.musical_key,
                    ":mb_recording": merged.musicbrainz_recording_id,
                    ":mb_release": merged.musicbrainz_release_id,
                    ":mb_release_group": merged.musicbrainz_release_group_id,
                    ":mb_artist": merged.musicbrainz_artist_id,
                    ":mb_release_artist": merged.musicbrainz_release_artist_id,
                    ":mb_work": merged.musicbrainz_work_id,
                    ":mb_track": merged.musicbrainz_track_id,
                    ":acoustid_id": merged.acoustid_id,
                    ":acoustid_score": acoustid_score.or(song.acoustid_score),
                    ":fingerprint": fingerprint,
                    ":now": now,
                    ":source": source,
                    ":confidence": outcome.confidence,
                    ":status": status,
                    ":error": outcome.error,
                    ":id": song_id,
                },
            ) {
                return Err(format!("DB update failed: {e}"));
            }
            outcome.database_updated = true;

            if cfg.write_tags {
                match write_file_tags(&song.path, &merged) {
                    Ok(()) => outcome.file_updated = true,
                    Err(e) => {
                        warn!("tag write failed for {}: {e}", song.path);
                        outcome.file_error = Some(e.clone());
                        let _ = conn.execute(
                            "UPDATE songs SET metadata_error = ?1 WHERE id = ?2",
                            params![format!("tag write failed: {e}"), song_id],
                        );
                    }
                }
            }
        }
        "ambiguous" | "not_found" | "error" => {
            let msg = outcome.error.clone();
            mark_checked(&conn, song_id, status, msg.as_deref());
            // Keep a fingerprint we generated even when no match was found;
            // it makes the next scan/--force run cheap.
            if let Some(fp) = fingerprint.as_deref() {
                let _ = conn.execute(
                    "UPDATE songs SET acoustid_fingerprint = ?1, acoustid_last_checked_at = ?2 WHERE id = ?3",
                    params![fp, now, song_id],
                );
            }
        }
        _ => {}
    }

    info!(
        "Recognition {}: {} ({:?} via {})",
        song_id, song.name, outcome.status, outcome.source.as_deref().unwrap_or("-")
    );
    Ok(outcome)
}

fn mark_checked(conn: &Connection, song_id: i64, status: &str, error: Option<&str>) {
    let now = unix_secs();
    if let Err(e) = conn.execute(
        "UPDATE songs SET metadata_status = ?1, metadata_error = ?2,
             metadata_last_checked_at = ?3
         WHERE id = ?4",
        params![status, error, now, song_id],
    ) {
        warn!("could not mark metadata status for song {song_id}: {e}");
    }
}

// ---------------------------------------------------------------------
// Tag writing (lofty)
// ---------------------------------------------------------------------

fn set_text(tag: &mut lofty::tag::Tag, key: lofty::tag::ItemKey, value: &Option<String>) {
    if let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        tag.insert_text(key, v.to_string());
    }
}

fn set_num(tag: &mut lofty::tag::Tag, key: lofty::tag::ItemKey, value: Option<i64>) {
    if let Some(v) = value {
        tag.insert_text(key, v.to_string());
    }
}

fn apply_tags(tag: &mut lofty::tag::Tag, t: &TrackTags) {
    use lofty::tag::{Accessor, ItemKey};

    if let Some(v) = t.title.clone().filter(|v| !v.trim().is_empty()) {
        tag.set_title(v);
    }
    if let Some(v) = t.artist.clone().filter(|v| !v.trim().is_empty()) {
        tag.set_artist(v);
    }
    if let Some(v) = t.album.clone().filter(|v| !v.trim().is_empty()) {
        tag.set_album(v);
    }
    if let Some(v) = t.genre.clone().filter(|v| !v.trim().is_empty()) {
        tag.set_genre(v);
    }
    if let Some(v) = t.track_number.and_then(|v| u32::try_from(v).ok()) {
        tag.set_track(v);
    }
    if let Some(v) = t.track_total.and_then(|v| u32::try_from(v).ok()) {
        tag.set_track_total(v);
    }
    if let Some(v) = t.disc_number.and_then(|v| u32::try_from(v).ok()) {
        tag.set_disk(v);
    }
    if let Some(v) = t.disc_total.and_then(|v| u32::try_from(v).ok()) {
        tag.set_disk_total(v);
    }
    if let Some(v) = t.release_year.and_then(|v| u32::try_from(v).ok()) {
        tag.set_year(v);
    }

    set_text(tag, ItemKey::AlbumArtist, &t.album_artist);
    set_text(tag, ItemKey::Composer, &t.composer);
    set_text(tag, ItemKey::Writer, &t.songwriter);
    set_text(tag, ItemKey::Arranger, &t.arranger);
    set_text(tag, ItemKey::Performer, &t.performer);
    set_text(tag, ItemKey::RecordingDate, &t.release_date);
    set_text(tag, ItemKey::OriginalReleaseDate, &t.original_release_date);
    set_text(tag, ItemKey::Isrc, &t.isrc);
    set_text(tag, ItemKey::Label, &t.label);
    set_text(tag, ItemKey::CatalogNumber, &t.catalog_number);
    set_text(tag, ItemKey::Barcode, &t.barcode);
    set_text(tag, ItemKey::Language, &t.language);
    set_text(tag, ItemKey::InitialKey, &t.musical_key);
    set_text(tag, ItemKey::MusicBrainzRecordingId, &t.musicbrainz_recording_id);
    set_text(tag, ItemKey::MusicBrainzReleaseId, &t.musicbrainz_release_id);
    set_text(
        tag,
        ItemKey::MusicBrainzReleaseGroupId,
        &t.musicbrainz_release_group_id,
    );
    set_text(tag, ItemKey::MusicBrainzArtistId, &t.musicbrainz_artist_id);
    set_text(
        tag,
        ItemKey::MusicBrainzReleaseArtistId,
        &t.musicbrainz_release_artist_id,
    );
    set_text(tag, ItemKey::MusicBrainzWorkId, &t.musicbrainz_work_id);
    set_text(tag, ItemKey::MusicBrainzTrackId, &t.musicbrainz_track_id);
    set_num(tag, ItemKey::Bpm, t.bpm.map(|b| b.round() as i64));
    // No first-class AcoustID ItemKey in lofty 0.22 — use the de-facto
    // `ACOUSTID_ID` field where the container supports free-form keys.
    set_text(
        tag,
        ItemKey::Unknown("ACOUSTID_ID".to_string()),
        &t.acoustid_id,
    );
}

/// Write only the fields we actually know into the file's primary tag.
/// Existing tags/unknown fields are preserved; audio data is untouched.
/// The write is atomic (copy -> rewrite temp -> rename).
pub fn write_file_tags(path: &str, tags: &TrackTags) -> Result<(), String> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::TagExt;

    let src = Path::new(path);
    let parent = src.parent().ok_or("file has no parent directory")?;
    // Keep the audio extension on the temp copy: lofty (and its format
    // probe) determine the container from the extension as well as the
    // magic bytes.
    let stem = src
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("track");
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("");
    let tmp = parent.join(if ext.is_empty() {
        format!(".{stem}.sjntmp")
    } else {
        format!(".{stem}.sjntmp.{ext}")
    });

    std::fs::copy(src, &tmp).map_err(|e| format!("create temp copy: {e}"))?;
    let result = (|| -> Result<(), String> {
        let tagged = lofty::read_from_path(&tmp).map_err(|e| format!("read temp: {e}"))?;
        let tag_type = tagged.file_type().primary_tag_type();
        let mut tag = tagged
            .primary_tag()
            .filter(|t| t.tag_type() == tag_type)
            .cloned()
            .unwrap_or_else(|| lofty::tag::Tag::new(tag_type));
        apply_tags(&mut tag, tags);

        if tag_type == lofty::tag::TagType::Id3v2 {
            // Lofty 0.22's generic `Tag` -> ID3v2 save path drops the UFID
            // frame that carries `musicbrainz_recording_id`. Convert to the
            // format-specific tag and insert the Picard-compatible UFID
            // (owner `http://musicbrainz.org`) explicitly.
            use lofty::id3::v2::{Frame, Id3v2Tag, UniqueFileIdentifierFrame};
            let mut id3 = Id3v2Tag::from(tag);
            if let Some(rec) = tags
                .musicbrainz_recording_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.is_ascii())
            {
                let _ = id3.insert(Frame::UniqueFileIdentifier(
                    UniqueFileIdentifierFrame::new(
                        "http://musicbrainz.org".to_string(),
                        rec.as_bytes().to_vec(),
                    ),
                ));
            }
            id3.save_to_path(&tmp, WriteOptions::default())
                .map_err(|e| format!("save ID3v2 tags: {e}"))?;
        } else {
            tag.save_to_path(&tmp, WriteOptions::default())
                .map_err(|e| format!("save tags: {e}"))?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            std::fs::rename(&tmp, src).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("replace original: {e}")
            })?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Copy the DB's current metadata into the song file (GUI "sync" action).
pub fn sync_song_to_file(song_id: i64) -> Result<RecognizeOutcome, String> {
    let conn = metadata_conn()?;
    let song: Song = conn
        .query_row(
            "SELECT * FROM songs WHERE id = ?1",
            params![song_id],
            song_from_row,
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "song not found".to_string())?;
    let tags = tags_from_song(&song);
    let mut outcome = RecognizeOutcome {
        song_id,
        song_name: song.name.clone(),
        status: "synced".to_string(),
        source: song.source_type.clone(),
        ..Default::default()
    };
    match write_file_tags(&song.path, &tags) {
        Ok(()) => {
            outcome.file_updated = true;
            let now = unix_secs();
            let _ = conn.execute(
                "UPDATE songs SET metadata_last_updated_at = ?1 WHERE id = ?2",
                params![now, song_id],
            );
        }
        Err(e) => {
            outcome.status = "error".to_string();
            outcome.error = Some(e.clone());
            outcome.file_error = Some(e);
        }
    }
    Ok(outcome)
}

/// Spawn an explicit (user-triggered) recognition run, independent of the
/// auto-recognition setting. Used by the GUI/CLI single-song actions.
pub fn spawn_recognize(song_id: i64, force: bool) {
    let _ = std::thread::Builder::new()
        .name(format!("sjnmusic-meta-{song_id}"))
        .spawn(move || {
            if let Err(e) = recognize_song(song_id, force) {
                warn!("recognition for song {song_id} failed: {e}");
            }
        });
}

/// Spawn a DB -> file tag sync for one song.
pub fn spawn_sync_to_file(song_id: i64) {
    let _ = std::thread::Builder::new()
        .name(format!("sjnmusic-tagsync-{song_id}"))
        .spawn(move || {
            if let Err(e) = sync_song_to_file(song_id) {
                warn!("tag sync for song {song_id} failed: {e}");
            }
        });
}

/// Kick off background recognition for one freshly-imported/downloaded
/// song when automatic recognition is enabled.
pub fn maybe_recognize_async(song_id: i64) {
    let cfg = current_config();
    if !cfg.auto_recognize {
        return;
    }
    let _ = std::thread::Builder::new()
        .name(format!("sjnmusic-meta-{song_id}"))
        .spawn(move || match recognize_song(song_id, false) {
            Ok(o) => {
                if o.status == "error" {
                    warn!("auto-recognition for {} failed: {:?}", o.song_name, o.error);
                }
            }
            Err(e) => warn!("auto-recognition for song {song_id} failed: {e}"),
        });
}

// ---------------------------------------------------------------------
// Status summary for GUI/CLI
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetadataStatus {
    pub acoustid_configured: bool,
    pub musicbrainz_enabled: bool,
    pub fpcalc_available: bool,
    pub auto_recognize: bool,
    pub write_tags: bool,
    pub total_songs: i64,
    pub complete_songs: i64,
    pub missing_songs: i64,
}

pub fn status_summary() -> MetadataStatus {
    let cfg = current_config();
    let (total, complete) = metadata_conn()
        .ok()
        .and_then(|conn| {
            let total = conn
                .query_row("SELECT COUNT(*) FROM songs", [], |r| r.get::<_, i64>(0))
                .ok()?;
            let complete = conn
                .query_row(
                    "SELECT COUNT(*) FROM songs
                     WHERE metadata_status = 'recognized'
                       AND title IS NOT NULL AND TRIM(title) != ''
                       AND artist IS NOT NULL AND TRIM(artist) != ''
                       AND (musicbrainz_recording_id IS NOT NULL OR acoustid_id IS NOT NULL)",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .ok()?;
            Some((total, complete))
        })
        .unwrap_or((0, 0));
    MetadataStatus {
        acoustid_configured: cfg.acoustid_configured(),
        musicbrainz_enabled: cfg.use_musicbrainz,
        fpcalc_available: fpcalc_available(),
        auto_recognize: cfg.auto_recognize,
        write_tags: cfg.write_tags,
        total_songs: total,
        complete_songs: complete,
        missing_songs: (total - complete).max(0),
    }
}

// ---------------------------------------------------------------------
// Service tests
// ---------------------------------------------------------------------

/// Probe a service. Returns a human-readable message on success; `Err`
/// carries the actionable failure (bad key, unreachable, …).
pub fn test_service(service: &str) -> Result<String, String> {
    let cfg = current_config();
    let client = http_client(&cfg)?;
    match service.to_ascii_lowercase().as_str() {
        "musicbrainz" => {
            throttle(Service::MusicBrainz);
            let base = cfg.musicbrainz_endpoint.trim_end_matches('/');
            let resp = client
                .get(format!("{base}/artist"))
                .query(&[("query", "test"), ("limit", "1"), ("fmt", "json")])
                .send()
                .map_err(|e| format!("MusicBrainz unreachable: {e}"))?;
            if resp.status().is_success() {
                Ok("MusicBrainz reachable".to_string())
            } else {
                Err(format!("MusicBrainz returned HTTP {}", resp.status()))
            }
        }
        "acoustid" => {
            if !cfg.acoustid_configured() {
                return Err("no AcoustID API key configured".to_string());
            }
            throttle(Service::AcoustId);
            // A client-only lookup is rejected with a fingerprint error when
            // the key is valid; "invalid API key" is what we're detecting.
            let resp = client
                .get(&cfg.acoustid_endpoint)
                .query(&[("client", cfg.acoustid_api_key.as_str()), ("meta", "recordings")])
                .send()
                .map_err(|e| format!("AcoustID unreachable: {e}"))?;
            let text = resp
                .text()
                .map_err(|e| format!("AcoustID response: {e}"))?;
            let lower = text.to_ascii_lowercase();
            if lower.contains("invalid api key") || lower.contains("invalid client") {
                Err("AcoustID rejected the API key".to_string())
            } else {
                Ok("AcoustID reachable and API key accepted".to_string())
            }
        }
        other => Err(format!("unknown service {other:?}")),
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_source_extraction_handles_common_urls() {
        assert_eq!(
            crate::state::youtube_video_id("https://www.youtube.com/watch?v=abc123XYZ_-"),
            Some("abc123XYZ_-".to_string())
        );
        assert_eq!(
            crate::state::youtube_video_id("https://youtu.be/abc123?t=4"),
            Some("abc123".to_string())
        );
        assert_eq!(
            crate::state::youtube_video_id("https://www.youtube.com/shorts/xyz"),
            Some("xyz".to_string())
        );
        assert_eq!(crate::state::youtube_video_id("not a url"), None);
    }

    #[test]
    fn merge_never_clears_existing_values() {
        let mut detected = TrackTags {
            title: Some("New title".to_string()),
            ..Default::default()
        };
        let existing = TrackTags {
            title: Some("Old title".to_string()),
            artist: Some("Old artist".to_string()),
            ..Default::default()
        };
        detected.fill_from(&existing);
        assert_eq!(detected.title.as_deref(), Some("New title"));
        assert_eq!(detected.artist.as_deref(), Some("Old artist"));
        assert!(detected.album.is_none());
    }

    #[test]
    fn fingerprint_roundtrip() {
        let raw = encode_fingerprint(123.5, "AQADtE");
        assert_eq!(decode_fingerprint(&raw), Some((123.5, "AQADtE".to_string())));
        assert_eq!(decode_fingerprint("garbage"), None);
    }

    #[test]
    fn acoustid_ties_are_detected_by_parser() {
        let v: Value = serde_json::from_str(
            r#"{"status":"ok","results":[
                {"id":"a1","score":0.95,"recordings":[
                    {"id":"r1","title":"One","artists":[{"id":"x","name":"Artist"}],
                     "releasegroups":[{"id":"g1","type":"Album"}]}]},
                {"id":"a2","score":0.95,"recordings":[
                    {"id":"r2","title":"Two","artists":[{"id":"y","name":"Artist"}],
                     "releasegroups":[{"id":"g2","type":"Album"}]}]}
            ]}"#,
        )
        .unwrap();
        let candidates = parse_acoustid(&v);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].score, 0.95);
    }

    #[test]
    fn musicbrainz_parse_extracts_release_and_track() {
        let v: Value = serde_json::from_str(
            r#"{
              "id": "rec-1",
              "title": "Song",
              "artist-credit": [{"name": "Artist", "artist": {"id": "art-1"}}],
              "isrcs": ["USAAA0000001"],
              "tags": [{"name": "rock"}],
              "releases": [{
                "id": "rel-1",
                "title": "Album",
                "date": "1999-05-01",
                "country": "US",
                "barcode": "123",
                "status": "Official",
                "artist-credit": [{"name": "Artist", "artist": {"id": "art-1"}}],
                "release-group": {"id": "rg-1", "primary-type": "Album",
                                  "first-release-date": "1999-04-30"},
                "label-info": [{"label": {"name": "Label"}, "catalog-number": "CAT1"}],
                "media": [{"position": 1, "track-count": 10, "tracks": [
                    {"id": "trk-1", "position": 3, "recording": {"id": "rec-1"}}
                ]}]
              }]
            }"#,
        )
        .unwrap();
        let t = tags_from_mb(&v, "rec-1");
        assert_eq!(t.title.as_deref(), Some("Song"));
        assert_eq!(t.album.as_deref(), Some("Album"));
        assert_eq!(t.musicbrainz_release_id.as_deref(), Some("rel-1"));
        assert_eq!(t.musicbrainz_release_group_id.as_deref(), Some("rg-1"));
        assert_eq!(t.musicbrainz_artist_id.as_deref(), Some("art-1"));
        assert_eq!(t.musicbrainz_track_id.as_deref(), Some("trk-1"));
        assert_eq!(t.track_number, Some(3));
        assert_eq!(t.track_total, Some(10));
        assert_eq!(t.disc_number, Some(1));
        assert_eq!(t.release_year, Some(1999));
        assert_eq!(t.isrc.as_deref(), Some("USAAA0000001"));
        assert_eq!(t.release_type.as_deref(), Some("Album"));
    }

    #[test]
    fn writes_and_reads_tags_atomically() {
        fn minimal_wav() -> Vec<u8> {
            let data_len: u32 = 8;
            let mut v = Vec::new();
            v.extend_from_slice(b"RIFF");
            v.extend_from_slice(&(36 + data_len).to_le_bytes());
            v.extend_from_slice(b"WAVEfmt ");
            v.extend_from_slice(&16u32.to_le_bytes());
            v.extend_from_slice(&1u16.to_le_bytes()); // PCM
            v.extend_from_slice(&1u16.to_le_bytes()); // mono
            v.extend_from_slice(&8000u32.to_le_bytes());
            v.extend_from_slice(&8000u32.to_le_bytes());
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&8u16.to_le_bytes());
            v.extend_from_slice(b"data");
            v.extend_from_slice(&data_len.to_le_bytes());
            v.extend_from_slice(&[128u8; 8]);
            v
        }

        let dir = std::env::temp_dir().join(format!(
            "sjnmeta-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        std::fs::write(&path, minimal_wav()).unwrap();

        let tags = TrackTags {
            title: Some("Title X".to_string()),
            artist: Some("Artist Y".to_string()),
            album: Some("Album Z".to_string()),
            track_number: Some(2),
            track_total: Some(9),
            isrc: Some("USAAA0000001".to_string()),
            musicbrainz_recording_id: Some("rec-123".to_string()),
            acoustid_id: Some("ac-123".to_string()),
            ..Default::default()
        };
        write_file_tags(path.to_str().unwrap(), &tags).unwrap();
        let read = read_file_tags(path.to_str().unwrap()).unwrap();
        assert_eq!(read.title.as_deref(), Some("Title X"));
        assert_eq!(read.artist.as_deref(), Some("Artist Y"));
        assert_eq!(read.album.as_deref(), Some("Album Z"));
        assert_eq!(read.track_number, Some(2));
        assert_eq!(read.track_total, Some(9));
        assert_eq!(read.isrc.as_deref(), Some("USAAA0000001"));
        // The temp file is renamed away, never left behind.
        assert!(!dir.join(".tone.sjntmp.wav").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MP3 is the primary library format (yt-dlp output). Generate a tiny
    /// file with ffmpeg when it is available (it is already a runtime
    /// dependency of the download pipeline) and verify that the
    /// MusicBrainz identifiers survive a write/read round-trip through
    /// ID3v2. Skipped on systems without ffmpeg.
    #[test]
    fn mp3_musicbrainz_ids_roundtrip_when_ffmpeg_available() {
        let dir = std::env::temp_dir().join(format!(
            "sjnmeta-mp3-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.mp3");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.4",
                "-q:a",
                "9",
            ])
            .arg(&path)
            .status();
        let ok = generated.map(|s| s.success()).unwrap_or(false);
        if !ok {
            eprintln!("ffmpeg unavailable; skipping MP3 tag round-trip test");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let tags = TrackTags {
            title: Some("MP3 Title".to_string()),
            artist: Some("MP3 Artist".to_string()),
            album: Some("MP3 Album".to_string()),
            musicbrainz_recording_id: Some("rec-123".to_string()),
            musicbrainz_release_id: Some("rel-123".to_string()),
            musicbrainz_artist_id: Some("art-123".to_string()),
            musicbrainz_release_group_id: Some("rg-123".to_string()),
            ..Default::default()
        };
        write_file_tags(path.to_str().unwrap(), &tags).unwrap();
        let read = read_file_tags(path.to_str().unwrap()).unwrap();
        assert_eq!(read.title.as_deref(), Some("MP3 Title"));
        assert_eq!(read.artist.as_deref(), Some("MP3 Artist"));
        assert_eq!(
            read.musicbrainz_recording_id.as_deref(),
            Some("rec-123")
        );
        assert_eq!(read.musicbrainz_release_id.as_deref(), Some("rel-123"));
        assert_eq!(read.musicbrainz_artist_id.as_deref(), Some("art-123"));
        assert_eq!(
            read.musicbrainz_release_group_id.as_deref(),
            Some("rg-123")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_columns_are_not_in_metadata_update() {
        // Guard against future edits accidentally adding `source_*` to the
        // recognition UPDATE: the only place that may write them is the
        // import/download path.
        let source = include_str!("metadata.rs");
        let update_start = source
            .find("UPDATE songs SET\n                    title = COALESCE")
            .expect("recognition update present");
        let update_end = source[update_start..]
            .find("metadata_error = :error")
            .map(|i| update_start + i)
            .unwrap_or(source.len());
        let update = &source[update_start..update_end];
        assert!(!update.contains("source_type"));
        assert!(!update.contains("source_url"));
        assert!(!update.contains("source_path"));
    }
}
