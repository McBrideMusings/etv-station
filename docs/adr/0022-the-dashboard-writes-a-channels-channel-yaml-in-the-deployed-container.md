# The dashboard writes a channel's channel.yaml in the deployed container

The deployed container's `channel.yaml` is the source of truth for a channel's programming, and the dashboard process (`etv-station --serve-ui`) is the one writer besides a redeploy. Apply loads the text with the daemon's own loader, copies the old file to `<file>.bak-<UTC stamp>`, writes the new text atomically and sends the daemon SIGHUP; the page reports applied or reverted from the daemon's reload record. The same dispatch is served to the page and to MCP at `/mcp`.

The write route has no authentication of its own, like every other dashboard route: Tailscale and the reverse proxy are the boundary, and nothing else reaches port 4747. Channels is a read-only inspector; Edit is its own page. A channel can be edited into a state the loader drops, and the page says so rather than refusing.
