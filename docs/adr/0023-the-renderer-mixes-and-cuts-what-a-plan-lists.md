# The renderer mixes and cuts what a plan lists; every arrangement is a plugin's

A render plan lists audio layers and picture cuts, nothing else. An audio layer names a source file or one of its stems (vocals, drums, bass, other, or the full mix), the range read, where it lands, its tempo and pitch, its gain, its fades with their curves, and any filters with their automation; a picture cut names a source, the range read and where it lands.

The renderer separates, stretches, filters, mixes, cuts and encodes exactly that, and holds no concept of a transition, recipe, seam, hook or song structure. Which songs play, which stems, how two songs join and what is on screen are decided by the plugin that writes the plan, so a new kind of mix is a plugin change, never a Rust change.

The facts a plugin decides from (per-stem loudness over time, vocal lines, beats) are measured by plex-db-ex's audio harvest and reach a plugin as the item's audio facts, which the station reads as opaque JSON.
