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
  - title: Composable sequencing
    details: Blocks of entries or pool/pattern interleaves resolve into one ordered list per generation, materialized forward. Recurring grids and live-event injection follow in v2+.
  - title: Schema-locked to ETV-next
    details: Depends on ETV-next's `ersatztv-playout` Rust crate via a git submodule. Schema drift becomes a compile-time question, not a runtime surprise.
---
