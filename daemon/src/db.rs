use log::{debug, info};
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
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT UNIQUE NOT NULL,
            path TEXT NOT NULL
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
             ON playlist_songs(playlist_id, position);",
    )
    .expect("Failed to create DB schema");

    debug!("Database initialized at {}", db_path());
    info!("Database ready");
    conn
}
