# etv-station reads another service's data only through its API

etv-station reads data another program owns only through the API that program publishes: an HTTP route, a CLI's output, a plugin's `ctx.http`. It opens no database or data file another program writes, and links no crate that reads one. plex-db-ex's facts — audio facts, enrichment, keywords, edges, taste vectors, Frame index shots — come from its explorer at `PLEXDB_EX_URL`. ADR 0014 places a plugin's own service calls in the plugin; this decision covers the core's reads as well.
