//! Background metadata resolution.
//!
//! After a playlist update lands, series, VOD and live entries still need their
//! detail fetched from the provider. This is the worker that does it: a queue of
//! per-input tasks, retry state persisted across restarts, and the probing that
//! fills in stream properties.
//!
//! It reads the running server through [`ctx::MetadataUpdateCtx`] and names
//! nothing else about it. `tuliprox-processing` states what it needs from this
//! worker as `MetadataUpdateSink`; the implementation is here, beside the type.

// Auto-trait resolution for this crate's deeply nested async call chains
// exceeds the default 128-step recursion limit. Without this, rustc emits
// `recursion_depth_exceeding_limit`, which is on its way to becoming a hard
// error (rust-lang/rust#159228).
#![recursion_limit = "256"]

pub mod ctx;
pub mod manager;

pub use self::{ctx::*, manager::*};
