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

pub mod download;
pub mod recording;

pub use self::{download::*, recording::*};
