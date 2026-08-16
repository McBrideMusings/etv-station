//! Catalog sqlite schema and up-only migrations (#47 locked identity model).
//!
//! Migrations are an ordered list; a migration's 1-based index is its version.
//! On open we read `schema_version` (0 if absent), then apply every migration
//! whose version exceeds it, recording the new version. Migrations are never
//! edited once shipped — only appended — so an existing `catalog.db` upgrades
//! deterministically.

use rusqlite::Connection;

use super::error::CatalogError;

/// Ordered migrations. Append only; never edit a shipped entry.
pub const MIGRATIONS: &[&str] = &[
    // v1 — initial catalog: deduped identity + tags + collections.
    r#"
    CREATE TABLE entries (
        entry_id         TEXT PRIMARY KEY,
        type             TEXT NOT NULL,
        title            TEXT NOT NULL,
        title_sort       TEXT,
        show             TEXT,
        show_id          TEXT,
        season           INTEGER,
        episode          INTEGER,
        absolute_episode INTEGER,
        edition          TEXT,
        studio           TEXT,
        year             INTEGER,
        release_date     TEXT,
        duration_ms      INTEGER,
        content_rating   TEXT,
        primary_source   TEXT NOT NULL,
        raw_metadata     TEXT
    );

    CREATE TABLE entry_sources (
        source        TEXT NOT NULL,
        source_id     TEXT NOT NULL,
        entry_id      TEXT NOT NULL REFERENCES entries(entry_id) ON DELETE CASCADE,
        playback_path TEXT NOT NULL,
        last_seen     TEXT,
        PRIMARY KEY (source, source_id)
    );
    CREATE INDEX idx_entry_sources_entry ON entry_sources(entry_id);

    CREATE TABLE entry_external_ids (
        namespace TEXT NOT NULL,
        value     TEXT NOT NULL,
        entry_id  TEXT NOT NULL REFERENCES entries(entry_id) ON DELETE CASCADE,
        PRIMARY KEY (namespace, value)
    );
    CREATE INDEX idx_entry_external_ids_entry ON entry_external_ids(entry_id);

    CREATE TABLE tags (
        entry_id  TEXT NOT NULL REFERENCES entries(entry_id) ON DELETE CASCADE,
        namespace TEXT NOT NULL,
        value     TEXT NOT NULL,
        PRIMARY KEY (entry_id, namespace, value)
    );
    CREATE INDEX idx_tags_ns_value ON tags(namespace, value);

    CREATE TABLE collections (
        collection_id TEXT PRIMARY KEY,
        name          TEXT NOT NULL,
        source        TEXT NOT NULL
    );

    CREATE TABLE collection_items (
        collection_id TEXT NOT NULL REFERENCES collections(collection_id) ON DELETE CASCADE,
        entry_id      TEXT NOT NULL REFERENCES entries(entry_id) ON DELETE CASCADE,
        position      INTEGER NOT NULL,
        PRIMARY KEY (collection_id, entry_id)
    );
    CREATE INDEX idx_collection_items_entry ON collection_items(entry_id);
    "#,
    // v2 — ingest bookkeeping. A single key/value table so the catalog carries
    // its own "when was this last filled, and from where" rather than the daemon
    // having to guess from row counts or file mtimes. Read at startup to decide
    // between skipping the ingest, asking Plex only for what changed, and a full
    // pass. Deliberately generic: the next thing worth remembering about an
    // ingest is another row, not another migration.
    r#"
    CREATE TABLE catalog_meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // v3 — the library an entry came from (#128). Holds the Plex section
    // *title* ("4K Movies"), not its numeric section key, so a channel reads
    // `item.library == "4K Movies"` rather than `item.library == "1"`. The
    // rename hazard is accepted deliberately: renaming a library in Plex makes
    // every expression naming the old title resolve to nothing, silently.
    // NULL for entries no library authored — everything the `fs` ingester
    // writes.
    r#"
    ALTER TABLE entries ADD COLUMN library TEXT;
    "#,
    // v4 — drop the stored `fs_dir` tags (#123). The filesystem scan used to
    // record each file's folder as a tag, but `add_tag` only ever inserts, so an
    // entry kept every folder any of its files had ever been in: move
    // `station-bumper-01.mp4` from `bumpers/` to `commercials/` and it answered
    // to both. `item.fs_dir` now computes the folder from
    // `entry_sources.playback_path` when the query runs, so these rows are read
    // by nothing. Deleting them here rather than in the ingest loop keeps it a
    // one-off for catalogs written by an older binary, instead of a sweep every
    // scan has to carry forever.
    r#"
    DELETE FROM tags WHERE namespace = 'fs_dir';
    "#,
    // v5 — the entry's synopsis (#186). Plex's `summary` attribute, stored
    // verbatim and fed to `ProgramMetadata.description` so the XMLTV guide
    // carries a `<desc>`. NULL for entries no source authored a summary for —
    // everything the `fs` ingester writes, and any Plex item Plex itself left
    // blank.
    r#"
    ALTER TABLE entries ADD COLUMN summary TEXT;
    "#,
    // v6 — artwork cache bookkeeping (#187). `artwork_cache_path` is the
    // filename (relative to the station's `artwork_cache_dir`) the entry's
    // fetched image is written under; `artwork_source_key` is the Plex
    // `thumb` path last used to fetch it, kept so a re-ingest can tell "art
    // changed" from "art unchanged" without re-downloading every item on
    // every pass. Both are written by the artwork-cache step alone —
    // `upsert_entry` never touches them — so a Plex metadata refresh can
    // never clobber a cached image out from under a pending write.
    r#"
    ALTER TABLE entries ADD COLUMN artwork_cache_path TEXT;
    ALTER TABLE entries ADD COLUMN artwork_source_key TEXT;
    "#,
    // v7 — soft delete (ADR 0006). An ingest pass that can no longer find a
    // source, or an entry all of whose sources are gone, writes an ISO-8601
    // timestamp here instead of issuing a `DELETE`. `NULL` = live, which is
    // what every existing row gets on upgrade — correct, since anything the
    // old binary would have deleted is already gone.
    //
    // Nullable with no default beyond `NULL` on purpose: "was this touched by
    // the last full pass" is the same bookkeeping `entry_sources.last_seen`
    // already does, and this is its missing half.
    r#"
    ALTER TABLE entries ADD COLUMN missing_since TEXT;
    ALTER TABLE entry_sources ADD COLUMN missing_since TEXT;
    "#,
];

/// The version the current binary's schema corresponds to.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Bring `conn` up to [`SCHEMA_VERSION`], applying each pending migration in a
/// single transaction so a failure leaves the version unchanged.
pub fn apply(conn: &Connection) -> Result<(), CatalogError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;

    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;

    for (i, ddl) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(ddl)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [version],
        )?;
        tx.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bring a fresh connection up to `version` only, so a test can stand where
    /// an older binary left a real `catalog.db`.
    fn at_version(version: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL);")
            .unwrap();
        for (i, ddl) in MIGRATIONS.iter().take(version).enumerate() {
            conn.execute_batch(ddl).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [(i + 1) as i64],
            )
            .unwrap();
        }
        conn
    }

    /// A catalog written by a binary that still stored the folder as a tag has
    /// rows saying a bumper is in `bumpers/` *and* `commercials/` — it was moved
    /// and the old tag never went away. Nothing reads those rows now, so the
    /// upgrade takes them out; the genres beside them, which Plex still authors,
    /// have to survive untouched.
    #[test]
    fn upgrading_drops_stored_fs_dir_tags_and_keeps_the_rest() {
        let conn = at_version(3);
        conn.execute_batch(
            "INSERT INTO entries (entry_id, type, title, primary_source)
                 VALUES ('fs:1', 'bumper', 'station-bumper-01', 'local_fs');
             INSERT INTO tags (entry_id, namespace, value) VALUES
                 ('fs:1', 'fs_dir', 'bumpers'),
                 ('fs:1', 'fs_dir', 'commercials'),
                 ('fs:1', 'genre', 'Ident');",
        )
        .unwrap();

        apply(&conn).unwrap();

        let remaining: Vec<String> = conn
            .prepare("SELECT namespace || '=' || value FROM tags ORDER BY 1")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(remaining, vec!["genre=Ident".to_string()]);
    }

    #[test]
    fn a_fresh_catalog_lands_on_the_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
