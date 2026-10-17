# A plugin declares its own capabilities; its channel grants exactly that set

A plugin's `capabilities()` function, inside the script, declares what it reaches beyond its ambient inputs: `catalog_read`, `watch_history`, `http`, a named datastore, and for a bumper `probe` and `read`. The channel grants capabilities to the script, and the station refuses to load a channel whose granted set differs from the declared set in either direction. A channel whose plugin declares no capability triggers no catalog query for it.
