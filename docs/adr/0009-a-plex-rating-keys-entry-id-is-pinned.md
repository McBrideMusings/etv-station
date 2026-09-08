# A Plex rating key's entry_id is pinned for life

Once `resolve_existing` has resolved a Plex item's `ratingKey` to an `entry_id` and written the `entry_sources` row recording that pairing, every later ingest pass for the same `ratingKey` resolves onto that same `entry_id` first, before any GUID or path match is consulted. Every GUID Plex currently reports for the item is still recorded against the pinned id, so it stays reachable by any of them — only the `entry_id` itself stops moving. This mirrors plex-db-ex's ADR-0008, which pins the same way on that project's side of the same catalog.
