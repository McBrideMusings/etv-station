//! The chapter renderer.
//!
//! A plan ([`document::Document`]: a chapter plan, [`plan::Plan`], or a set plan,
//! [`set::SetPlan`]) goes in; a video file comes out. [`render::render_document`]
//! does one plan in the foreground; [`queue`] holds plans waiting for it and a
//! state file per job; [`cache`] keeps the rendered chapters under a size cap;
//! [`deps`] reports which of the programs it shells out to are present.
//!
//! The renderer knows nothing about why a plan looks the way it does. Choosing
//! hooks and writing plans is a plugin's policy; this crate only turns the
//! document into audio and picture.

pub mod audio;
pub mod cache;
pub mod deps;
pub mod document;
pub mod error;
pub mod plan;
pub mod queue;
pub mod render;
pub mod set;
pub mod set_render;
pub mod settings;
pub mod stems;
pub mod tools;
pub mod video;

pub use document::Document;
pub use error::RenderError;
pub use plan::Plan;
pub use settings::Settings;
