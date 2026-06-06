use rusqlite::Connection;

use crate::paths::db_path;

pub fn init_db() {
    let conn = Connection::open(db_path()).unwrap();

    conn.execute(
        "CREATE TABLE IF NOT EXISTS songs (
            id INTEGER PRIMARY KEY,
            name TEXT UNIQUE,
            path TEXT
        )",
        [],
    ).unwrap();
}
