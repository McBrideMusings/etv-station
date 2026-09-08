# A plugin gets the channel's seed, and randomness is the only thing `ctx` widens for

`ctx.seed` carries the channel's resolved seed, mixed with the pool name, so a script can make a random choice that reproduces exactly on a second generation. It is the only value added to the plugin context for this purpose — a script still cannot read the clock, open a file, or ask the station to compute anything on its behalf.
