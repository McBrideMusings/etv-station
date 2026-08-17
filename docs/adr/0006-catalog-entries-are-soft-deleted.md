# A catalog entry is marked missing, never deleted

`entries.missing_since` (and the same column on `entry_sources`) replaces the hard delete `fs.rs:188-193` performs today (`sweep_unseen_sources` + `delete_entries_without_sources`). A source or entry a full ingest pass can no longer find gets a timestamp instead of a `DELETE`. The row, its `entry_id`, and everything joined against that id stay put.

## Why

The immediate trigger: a rename on the Unraid media library (Radarr adding `[EN]`/`2.0` tags to filenames) left several channels airing black/silence, because a playout JSON file written before the rename had baked in the old `path` and nothing regenerated it. Fixing that only needs `entry_id` — already stable across a rename via GUID priority (`identity.rs:32-46`) — and a periodic sweep that re-resolves `path` from it.

But the same code path also runs when a file is genuinely gone, not renamed: deleted from disk, pulled from Plex. Today that's indistinguishable from a rename at the SQL layer — both are "the entry_id's provenance row didn't get touched by this pass" — and the existing `fs.rs` handling for that case is `DELETE`. That's the part this ADR changes.

The reason it has to change now rather than later: `entry_id` is meant to be a durable join key. Watch history (`history.rs`) already keys `airings` on it. The planned graph/enrichment work (`vendor/plexdb-reader/`, affinity edges, taste vectors) joins on it too. A hard delete on a temporarily-missing file — a drive that's briefly offline, a Radarr re-import mid-flight, a rename the ingest pass catches between the old and new filename — doesn't just drop a catalog row, it orphans every join built on that id, silently, the moment the delete runs. A soft mark is reversible; a `DELETE` is not.

## What we chose, and the rejected alternative

- **`missing_since: Option<String>` on both `entries` and `entry_sources`, cleared on the next ingest pass that sees the row again.** Mirrors `entry_sources.last_seen`, which already exists for the same "was this touched by the last pass" purpose — this isn't a new concept, it's the missing half of one already in the schema.

- **Keep the hard delete, rejected.** Simpler, and keeps `catalog.db` bounded — a title gone forever stays gone from the row count. Rejected because "gone forever" and "gone until the next full sweep re-matches it" are indistinguishable at delete time, and the failure mode of guessing wrong is silent data loss on every table that joins on `entry_id`, not a visible error.

## Consequences

**`catalog.db` grows monotonically for entries that never come back.** Nothing prunes a `missing_since` row today. A title permanently removed from the library stays in the catalog forever, taking up space and (until filtered) needing every future query to explicitly exclude it. Acceptable for now — the alternative is re-introducing the exact silent-loss risk this ADR exists to avoid — but a bounded external purge policy is future work if this ever matters at scale.

**Every read site that assumed "a catalog row exists" now has to ask whether it's missing.** The scheduler's pool-selection query (`query.rs`) gains `missing_since IS NULL`. Anything that joins `entries` without that filter — history, attribution, future graph work — gets missing entries back by default, which is correct for those (a missing title still has real watch history) and would be wrong if silently copied into a scheduling context.

**`fs.rs`'s `sweep_unseen_sources` / `delete_entries_without_sources` names stop matching what they do.** They mark now, not delete. Rename or remove them in the same change that implements this, rather than leaving a `delete_` function that sets a column.
