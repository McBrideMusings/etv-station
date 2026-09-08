# Plugin-authored content reaches an overlay only as opaque metadata, never a named field

A picked item's `metadata` — the opaque JSON a plugin attaches to it — rides untouched into `ProgramContext.metadata`, the one channel through which anything plugin-specific reaches an overlay script. No Rust code reads or interprets a key inside it; an overlay script that wants a value pulls it out itself. `ProgramContext`'s other fields stay typed and station-owned only for what is universal across every channel and every plugin — title, season, episode, year, elapsed/remaining time.
