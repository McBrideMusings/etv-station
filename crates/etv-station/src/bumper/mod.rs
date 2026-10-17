//! Runtime bumpers: a plugin returns a [`timeline::Timeline`] of flexible
//! segments, and [`timeline::fit`] resolves it onto a gap of known length.
//! [`hook`] runs the plugin's `build(ctx)`.

pub mod hook;
pub mod ledger;
pub mod timeline;
