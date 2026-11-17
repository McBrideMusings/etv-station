//! The renderer.
//!
//! A plan ([`plan::Plan`]: audio layers and picture cuts) goes in; a video file
//! comes out. [`render::render`] does one plan in the foreground; [`queue`] holds plans waiting for it and a
//! state file per job; [`cache`] keeps the rendered files under a size cap;
//! [`deps`] reports which of the programs it shells out to are present.
//!
//! The renderer knows nothing about why a plan looks the way it does: which
//! sources, which stems, how they join and what is on screen are a plugin's
//! choices, already resolved into layers and cuts (ADR 0023). This crate only
//! turns the plan into audio and picture.

pub mod audio;
pub mod cache;
pub mod deps;
pub mod error;
pub mod plan;
pub mod queue;
pub mod render;
pub mod settings;
pub mod stems;
pub mod tools;
pub mod video;

pub use error::RenderError;
pub use plan::Plan;
pub use settings::Settings;
