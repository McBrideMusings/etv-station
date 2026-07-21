//! Catalog ingesters — the units that *populate* the [`super::Catalog`] store
//! (the store itself is persistence-only). Each ingester walks a source (the
//! local filesystem here; the Plex API in a later slice), derives a deterministic
//! `entry_id` via [`super::identity`] with ingest-time **path-match inherit**, and
//! writes `entries` + `entry_sources` rows.

pub mod fs;
