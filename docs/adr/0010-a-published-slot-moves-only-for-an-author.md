---
applies-to:
  - crates/etv-station/src/daemon.rs
  - crates/etv-station/src/resume.rs
  - crates/etv-station/src/resolve.rs
---

# A published slot moves only for an author

The regeneration fingerprint (`channel_input_fingerprint`) hashes a channel's config bytes and resolved overlay bytes, and nothing else. Catalog drift — a film arriving in Plex, an item crossing the watched boundary — never rewinds an already-published schedule; new content reaches the screen at the window frontier on the ordinary roll tick, not by rewriting a span a viewer has already been shown. A slot whose catalog entry is broken (`missing_since` set) is the one exception: the startup path rewinds from that item's own `start`, narrower than the earliest unaired checkpoint, since the published schedule there is wrong rather than merely old.
