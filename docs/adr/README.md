# Architecture Decision Records

ADRs record non-obvious decisions made at a point in time, against the code as it then stood. They live here to explain why a choice was made and what made it necessary, not to describe the current state of the code.

## Handling drift

Code changes; ADRs don't. When drift occurs — a referenced function is renamed, removed, or moved — the ADR text stays as written. The decision was made against that historical code, and rewriting the reference would destroy the record.

Instead, add a short dated note directly below the affected reference, naming what happened and where the behaviour now lives:

> **Note, 2026-08:** `validate_overlay_configs` was removed in 6d2f39c (the station → channel → block overlay cascade). Overlay specs are now resolved and validated during `config::load` — `config::overlay::resolve_channel`, called from `config/load.rs` — so there is no separate overlay-validation pass at `prepare_generation` time.

This keeps the historical reference visible while making the current picture clear. **ADRs are historical snapshots; later drift is annotated, never edited away.**
