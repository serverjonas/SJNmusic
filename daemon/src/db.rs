use log::{debug, info, warn};
use rusqlite::Connection;

use crate::paths::{db_path, ensure_dirs};

/// Every metadata/source column added after the original `songs` table.
/// Kept in one list so migration is a single idempotent loop and tests
/// can assert the complete post-condition.
const SONG_COLUMNS: &[(&str, &str)] = &[
    // ---- MusicBrainz identifiers ------------------------------------
    ("musicbrainz_recording_id", "TEXT"),
    ("musicbrainz_release_id", "TEXT"),
    ("musicbrainz_release_group_id", "TEXT"),
    ("musicbrainz_artist_id", "TEXT"),
    ("musicbrainz_release_artist_id", "TEXT"),
    ("musicbrainz_work_id", "TEXT"),
    ("musicbrainz_track_id", "TEXT"),
    // ---- AcoustID ---------------------------------------------------
    ("acoustid_id", "TEXT"),
    ("acoustid_score", "REAL"),
    ("acoustid_fingerprint", "TEXT"),
    ("acoustid_last_checked_at", "INTEGER"),
    // ---- General metadata -------------------------------------------
    ("title", "TEXT"),
    ("artist", "TEXT"),
    ("album", "TEXT"),
    ("album_artist", "TEXT"),
    ("composer", "TEXT"),
    ("songwriter", "TEXT"),
    ("arranger", "TEXT"),
    ("performer", "TEXT"),
    ("release_date", "TEXT"),
    ("original_release_date", "TEXT"),
    ("release_year", "INTEGER"),
    ("track_number", "INTEGER"),
    ("track_total", "INTEGER"),
    ("disc_number", "INTEGER"),
    ("disc_total", "INTEGER"),
    ("genre", "TEXT"),
    ("tags", "TEXT"),
    ("style", "TEXT"),
    ("recording_location", "TEXT"),
    ("language", "TEXT"),
    ("isrc", "TEXT"),
    ("label", "TEXT"),
    ("catalog_number", "TEXT"),
    ("barcode", "TEXT"),
    ("release_type", "TEXT"),
    ("release_country", "TEXT"),
    ("bpm", "REAL"),
    ("musical_key", "TEXT"),
    // ---- Technical bookkeeping --------------------------------------
    ("metadata_source", "TEXT"),
    ("metadata_confidence", "REAL"),
    ("metadata_last_updated_at", "INTEGER"),
    ("metadata_last_checked_at", "INTEGER"),
    ("metadata_status", "TEXT"),
    ("metadata_error", "TEXT"),
    // ---- Original source (never touched by metadata updates) --------
    ("source_type", "TEXT"),
    ("source_url", "TEXT"),
    ("source_path", "TEXT"),
    ("source_identifier", "TEXT"),
    ("source_title", "TEXT"),
    ("source_platform", "TEXT"),
    ("source_created_at", "INTEGER"),
    ("source_metadata", "TEXT"),
    // ---- Re-import duplicate detection ------------------------------
    ("content_hash", "TEXT"),
];

/// Open (and configure) the SQLite database, creating tables on first run.
pub fn open_db() -> Connection {
    ensure_dirs();
    let conn = Connection::open(db_path()).expect("Failed to open songs DB");
    configure(&conn);
    init_schema(&conn);
    migrate(&conn);
    debug!("Database initialized at {}", db_path());
    info!("Database ready");
    conn
}

fn configure(conn: &Connection) {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )
    .expect("Failed to set DB pragmas");
}

/// Base schema for new installs. Additive migrations below bring older
/// databases to the same shape without touching existing rows.
pub fn init_schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS songs (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            name          TEXT UNIQUE NOT NULL,
            path          TEXT NOT NULL,
            duration_secs INTEGER
         );

        CREATE TABLE IF NOT EXISTS queue (
            position INTEGER PRIMARY KEY AUTOINCREMENT,
            song_id  INTEGER NOT NULL,
            FOREIGN KEY(song_id) REFERENCES songs(id) ON DELETE CASCADE
         );

        CREATE TABLE IF NOT EXISTS playlists (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT UNIQUE NOT NULL
         );

        CREATE TABLE IF NOT EXISTS playlist_songs (
            playlist_id INTEGER NOT NULL,
            position    INTEGER NOT NULL,
            song_id     INTEGER NOT NULL,
            PRIMARY KEY (playlist_id, position),
            FOREIGN KEY(playlist_id) REFERENCES playlists(id) ON DELETE CASCADE,
            FOREIGN KEY(song_id)     REFERENCES songs(id)     ON DELETE CASCADE
         );

         CREATE INDEX IF NOT EXISTS idx_playlist_songs_pl
             ON playlist_songs(playlist_id, position);

        CREATE TABLE IF NOT EXISTS history (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            song_id              INTEGER NOT NULL,
            played_at            INTEGER NOT NULL,
            duration_secs_played INTEGER NOT NULL,
            FOREIGN KEY(song_id) REFERENCES songs(id) ON DELETE CASCADE
         );

        CREATE INDEX IF NOT EXISTS idx_history_played_at
            ON history(played_at DESC);
        CREATE INDEX IF NOT EXISTS idx_history_song_id
            ON history(song_id);

        CREATE TABLE IF NOT EXISTS metadata_cache (
            cache_key  TEXT PRIMARY KEY,
            payload    TEXT NOT NULL,
            fetched_at INTEGER NOT NULL
         );",
    )
    .expect("Failed to create DB schema");
}

/// Idempotently add every post-v1 column. SQLite has no `ADD COLUMN IF
/// NOT EXISTS`, so the duplicate-column error is swallowed — its presence
/// is the intended post-condition. Never drops or rewrites anything, so
/// existing data and foreign keys survive.
pub fn migrate(conn: &Connection) {
    add_column_if_missing(conn, "songs", "duration_secs", "INTEGER");
    for (column, decl) in SONG_COLUMNS {
        add_column_if_missing(conn, "songs", column, decl);
    }
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_songs_metadata_status
             ON songs(metadata_status);
         CREATE INDEX IF NOT EXISTS idx_songs_content_hash
             ON songs(content_hash);
         CREATE INDEX IF NOT EXISTS idx_songs_mb_recording
             ON songs(musicbrainz_recording_id);",
    );
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) {
    let stmt = format!("ALTER TABLE {table} ADD COLUMN {column} {decl}");
    match conn.execute_batch(&stmt) {
        Ok(_) => info!("Added column {table}.{column}"),
        Err(e) => {
            // "duplicate column name: <name>" is the expected no-op signal.
            let msg = e.to_string();
            if msg.contains("duplicate column name") {
                debug!("Column {table}.{column} already present");
            } else {
                warn!("Failed to add column {table}.{column}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Migration must be additive: an old DB with rows keeps its data and
    /// gains every new nullable column.
    #[test]
    fn migration_preserves_existing_rows_and_adds_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE songs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                path TEXT NOT NULL
             );
             INSERT INTO songs (name, path) VALUES ('Old Song', '/tmp/old.mp3');",
        )
        .unwrap();

        migrate(&conn);
        // Running twice must also be safe (every daemon start does this).
        migrate(&conn);

        let (id, name): (i64, String) = conn
            .query_row("SELECT id, name FROM songs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(name, "Old Song");

        let mut stmt = conn.prepare("PRAGMA table_info(songs)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for (column, _) in SONG_COLUMNS {
            assert!(cols.iter().any(|c| c == column), "missing column {column}");
        }
        let cache_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='metadata_cache'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // The test only created `songs`, so metadata_cache is added by
        // `init_schema`, not `migrate`; verify migrate itself didn't error.
        assert_eq!(cache_exists, 0);
    }
}
