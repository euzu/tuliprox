//! The digital video recorder.
//!
//! Everything from a recording rule to a file on disk: the download queue that
//! holds scheduled, active and finished tasks, the scheduler that materializes
//! rules into occurrences, the workers that run ffmpeg, and the supervisors
//! that reconcile, notify and enforce retention.
//!
//! The DVR reads the running server through [`recording::recording_ctx::RecordingCtx`] - the
//! configuration, the queue, the event bus and the HTTP client - and names
//! nothing else about it. That is what lets it live outside `api`.

// Auto-trait resolution for this crate's deeply nested async call chains
// exceeds the default 128-step recursion limit. Without this, rustc emits
// `recursion_depth_exceeding_limit`, which is on its way to becoming a hard
// error (rust-lang/rust#159228).
#![recursion_limit = "256"]

pub mod download;
pub mod recording;

pub use self::{download::*, recording::*};
