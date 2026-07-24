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
