# A Plex rating key's entry_id is pinned for life

Once `resolve_existing` (`crates/etv-station/src/catalog/ingest/plex.rs`) has resolved a Plex item's `ratingKey` to an `entry_id` and written the `entry_sources` row that records that pairing, every later ingest pass for the same `ratingKey` resolves onto that same `entry_id` first, before anything else is consulted. This mirrors `plex-db-ex`'s ADR-0008, which pins the same way on the Python side of the same problem.

## Why

`resolve_existing`'s precedence chain used to be: (1) a GUID the catalog already knows, (2) a canonical-path match, (3) fall through to a fresh derivation via `derive_entry_id`. Nothing in that chain consulted the `entry_sources` row a prior pass had already written for this exact `ratingKey`.

That is reachable in the ordinary course of a library staying scraped: Plex re-matches a title and its `Guid` list changes — commonly gaining a higher-priority namespace, e.g. a `tmdb:` GUID becoming an `imdb:` one once Plex finds a better match. On the next ingest pass, the new `imdb:` value is unknown to the catalog (step 1 misses), the canonical path may also have changed or already be claimed by the pinned entry rather than a fresh one (step 2 misses), and the pass falls through to `derive_entry_id`, which mints a brand-new id under the new GUID. The old id's `entries` row, its `entry_sources` row, and its `entry_external_ids` rows are still touched by `mark_unseen_sources_missing` or drift stale, while every join built on the old `entry_id` — watch history (`history.rs`), resume cursors (`resume.rs`), affinity edges and taste vectors (`vendor/plexdb-reader/`) — silently stops matching the entry that's actually airing now.

`plexdb-reader`'s side of this catalog already pins on `ratingKey` (that project's ADR-0008), so a granted Rhai plugin reading enrichment tags, affinity edges, or taste vectors through that crate was already keyed on the *old*, never-re-derived id. Without this change, station-side and plexdb-side identity for the same title can permanently disagree the moment Plex re-scrapes it.

## What we chose, and the rejected alternative

- **Pin by `ratingKey` first, chosen.** `resolve_existing` gains a first step: `catalog.entry_id_for_source(Source::Plex, &item.rating_key)`, checked before the GUID loop and the path match. `Catalog::entry_id_for_source` already existed and is already used elsewhere in this file (collection/label member resolution) — no new catalog API, no schema change. The rest of the ingest loop is unchanged: every currently-seen GUID is still recorded via `catalog.add_external_id` *after* resolution, so a pinned entry stays reachable by every GUID Plex has ever reported for it, old and new alike — only the `entry_id` itself stops moving.

- **Keep re-deriving every pass, rejected.** This is the status quo the bug report describes. It self-heals in the narrow case where a title's GUID set is stable, but is silently wrong the moment it isn't, and "wrong" here means an orphaned join on every table keyed by `entry_id`, discovered only when the symptom (wrong or missing watch history, wrong resume cursor) surfaces somewhere else.

- **Re-key everything downstream on rename instead of pinning upstream, rejected.** Propagating an `entry_id` change to `airings`, `resume`, and any future plexdb-reader join would require a migration on every ingest pass that changes an id, touching far more code than the one-branch fix, and still leaves a window where the two systems disagree until the migration runs.

## Consequences

**A pinned id can diverge from a fresh derivation forever, and nothing in this change ever reconciles that.** If a title's very first ingest pinned it under a weak or wrong GUID (e.g. Plex first matched it to the wrong tmdb entry before a manual re-match), the pin now preserves that mistake indefinitely — the self-healing that accidental re-derivation used to provide is exactly what this ADR removes. Correcting a genuinely wrong historical id requires a manual catalog edit; there is no automatic re-pin path.

**`entry_sources` rows survive soft-deletion (ADR 0006 marks missing, never deletes), so the pin outlives a title leaving and returning to Plex.** A `ratingKey` that disappears from the library and later reappears (re-added, a Plex library re-scan) keeps its original pinned `entry_id` rather than deriving a new one — intended, since the alternative is the exact same drift this ADR exists to close, but it means the pin has no expiry.

**`entry.show_id` is not covered by this pin and keeps re-deriving from show GUIDs on every pass** (`ingest_items`, `plex.rs:375-385` as of this change). It has the same drift exposure this ADR closes for the item-level `entry_id`, but #274 explicitly scoped that out; extending the pin there is a separate follow-up, not part of this decision.
