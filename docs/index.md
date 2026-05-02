---
layout: home

hero:
  name: etv-station
  text: Playout-JSON generator for ErsatzTV-next
  tagline: A standalone daemon that decides what to play and writes the JSON that drives it. Companion to ETV-next, not a fork.
  actions:
    - theme: brand
      text: Read the PRD
      link: /PRD
    - theme: alt
      text: Roadmap
      link: /roadmap
    - theme: alt
      text: Architecture
      link: /architecture

features:
  - title: Filesystem-only contract
    details: Writes playout JSON to a shared volume; ETV-next reads it. No IPC, no shared schema fork, no network coupling.
  - title: Plug-in sequencing rules
    details: v1 ships Loop Forever. The rule trait is designed for recurring grids, shuffle, hybrid, and live-event injection in v2+.
  - title: Schema-locked to ETV-next
    details: Depends on ETV-next's `ersatztv-playout` Rust crate via a git submodule. Schema drift becomes a compile-time question, not a runtime surprise.
---
