# A plugin declares its own capabilities; the station never mirrors the declaration

A plugin's `capabilities()` function, inside the script, is the only declaration of what it reads — currently `catalog_read`, gating `ctx.sets`. A pool's config carries no separate `capabilities:` field and the station performs no cross-check against one. A channel whose plugin declares no capability triggers no catalog query for it.
