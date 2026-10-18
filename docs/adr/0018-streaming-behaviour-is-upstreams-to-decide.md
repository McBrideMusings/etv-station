---
applies-to: ["vendor/etv-next/**"]
---

# Streaming behaviour is upstream's unless it strands a viewer

How video reaches a client is ETV-next's decision by default, and this repository treats upstream's choices as correct. A station-side problem that appears to call for changing one is answered on the station side or accepted.

The exception is a behaviour that leaves a real viewer stuck with no way back, where the cause is in how ETV-next serves or keeps HLS and the station side cannot reach it. Those changes are made in `vendor/etv-next/`, each in its own commit and tied to the incident it fixes: per-run segment folders (etv-station-262), the reburst gate (#339), fifteen-minute segment retention (etv-station-194), and deleting a root playlist in the same step that removes the last run folder it names (etv-station-9qgi), and treating an already-missing segment file as removed when trimming, so a playlist update is never skipped (etv-station-1foz).

Everything else this repository adds to `vendor/etv-next/` is what upstream does not do at all: the HDHomeRun tuner endpoints, `/artwork`, `/logo` (a channel logo the lineup names as a file, #342), per-channel failure reporting (the cause logged on every counted failure and `/health/channels.json`, etv-station-323), and the playout-JSON contract with the station.
