//! A plex-db-ex snapshot fixture for the audio-facts tests, shared by the lib's unit
//! tests and the integration tests through `#[path]`.

use std::path::Path;

/// Build a plex-db-ex snapshot at `path` holding `items`, each
/// `(item_id, facts)`: every item gets a `media_part`, and one with `Some`
/// facts also an `audio_part` and an `audio_facts` row (the v23/v28/v29
/// shape `Reader::audio_facts_for_many` joins).
pub fn write(path: &Path, items: &[(&str, Option<&str>)]) {
    let _ = std::fs::remove_file(path);
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version (version) VALUES ({shape});
         CREATE TABLE reader_shape (version INTEGER NOT NULL);
         INSERT INTO reader_shape (version) VALUES ({shape});
         CREATE TABLE items (item_id TEXT PRIMARY KEY, type TEXT NOT NULL);
         CREATE TABLE media_part (part_id INTEGER PRIMARY KEY, item_id TEXT NOT NULL,
             rating_key TEXT NOT NULL, file TEXT NOT NULL, fetched_at TEXT NOT NULL);
         CREATE TABLE audio_facts (file_hash TEXT PRIMARY KEY, facts TEXT NOT NULL,
             analyzed_at TEXT NOT NULL);
         CREATE TABLE audio_part (part_id INTEGER PRIMARY KEY, item_id TEXT NOT NULL,
             file_hash TEXT NOT NULL);",
        shape = plexdb_reader::SUPPORTED_READER_SHAPE,
    ))
    .unwrap();
    for (n, (item_id, facts)) in items.iter().enumerate() {
        let part = n as i64 + 1;
        conn.execute(
            "INSERT INTO items (item_id, type) VALUES (?1, 'movie')",
            [item_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_part (part_id, item_id, rating_key, file, fetched_at) \
             VALUES (?1, ?2, ?2, ?3, '2026-01-01T00:00:00+00:00')",
            rusqlite::params![part, item_id, format!("/m/{item_id}.mkv")],
        )
        .unwrap();
        if let Some(facts) = facts {
            let hash = format!("hash-{item_id}");
            conn.execute(
                "INSERT INTO audio_facts (file_hash, facts, analyzed_at) \
                 VALUES (?1, ?2, '2026-01-01T00:00:00+00:00')",
                rusqlite::params![hash, facts],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO audio_part (part_id, item_id, file_hash) VALUES (?1, ?2, ?3)",
                rusqlite::params![part, item_id, hash],
            )
            .unwrap();
        }
    }
}
