use log::{debug, info, warn};
use rusqlite::Connection;

use crate::paths::{db_path, ensure_dirs};

/// Open (and configure) the SQLite database, creating tables on first run.
pub fn open_db() -> Connection {
    ensure_dirs();
    let conn = Connection::open(db_path()).expect("Failed to open songs DB");

    // Reasonable defaults for a small local DB.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .expect("Failed to set DB pragmas");

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
            ON history(song_id);",
    )
    .expect("Failed to create DB schema");

    // Idempotent column migrations for older DBs created before
    // `duration_secs` existed. SQLite raises a duplicate-column error which we
    // swallow because the column already being present is exactly the intended
    // post-condition of this step.
    add_column_if_missing(&conn, "songs", "duration_secs", "INTEGER");

    debug!("Database initialized at {}", db_path());
    info!("Database ready");
    conn
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
