# A scorer plugin replaces a pool's `expr`, not its `order`

A pool names either a CEL expression or a plugin script, never both. A plugin pool owns both gathering candidates and ranking them, and returns an ordered list of `entry_id`s; everything downstream — `select`, `rotate`, `advance`, `on_short`, and the pattern's `take` — operates on that list exactly as it does on a CEL-resolved one. The script never returns scores, only ids; the ranking stays internal to the plugin.
