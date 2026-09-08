# The reason set and the audit trail are separate fields

A pick's viewer-facing justification and its diagnostic justification are recorded in two sibling keys of the same `metadata` blob: `reason_set` for the why line an overlay renders on screen, `audit` for the stage list a report reads (ADR 0011). Both are computed once by the scorer at pick time and written independently; nothing in the system keeps them from drifting apart. `why` is reserved in this project's vocabulary for the on-screen sentence — the audit trail's per-stage field is `verdict` for that reason.
