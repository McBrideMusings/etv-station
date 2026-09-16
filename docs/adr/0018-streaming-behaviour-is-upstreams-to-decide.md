---
applies-to: ["vendor/etv-next/**"]
---

# Streaming behaviour is upstream's to decide

How video reaches a client is ETV-next's decision, and this repository treats upstream's choices as correct. A station-side problem that appears to call for changing one is answered on the station side or accepted.

What this repository adds to `vendor/etv-next/` is confined to what upstream does not do at all: the HDHomeRun tuner endpoints, `/artwork`, and the playout-JSON contract with the station.
