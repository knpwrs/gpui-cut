//! Rendering the playlist with ffmpeg.
//!
//! Exports are first turned into an [`plan::ExportPlan`] — a list of ffmpeg
//! invocations — by pure functions that are easy to test. [`run`] then
//! executes a plan step by step, reporting progress and honoring cancellation.

pub mod args;
pub mod naming;
pub mod options;
pub mod plan;
pub mod run;

pub use options::*;
