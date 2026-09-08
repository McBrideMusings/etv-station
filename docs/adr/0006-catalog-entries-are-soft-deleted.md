# A catalog entry is marked missing, never deleted

`entries.missing_since` (and the same column on `entry_sources`) replaces a hard delete: a source or entry a full ingest pass can no longer find gets a timestamp instead of a `DELETE`, cleared on the next pass that sees the row again. The row, its `entry_id`, and everything joined against that id stay in place. A read site that needs only currently-present entries filters on `missing_since IS NULL`; a site reading history or enrichment data does not, since a missing title still has a real past.
