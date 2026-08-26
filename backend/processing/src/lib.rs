//! The playlist processing pipeline.
//!
//! One target at a time: fetch each configured input, parse it, apply mappings
//! and filters, deduplicate, sort, resolve series and VOD detail, acquire EPG,
//! and hand the result to the repository.
//!
//! Everything here runs on a `PlaylistProcessingContext` rather than the
//! server's root state, which is what lets the pipeline live outside the
//! binary.

// Auto-trait resolution for this crate's deeply nested async call chains
// exceeds the default 128-step recursion limit. Without this, rustc emits
// `recursion_depth_exceeding_limit`, which is on its way to becoming a hard
// error (rust-lang/rust#159228).
#![recursion_limit = "256"]

// EPG acquisition for a configured input. It needs a `PlaylistProcessingContext`
// and repository storage paths, so it belongs on this side of the boundary -
// living in `utils` made that module depend on both layers above it.
// Wraps a `PlaylistSource` and a `TVGuide` for the duration of one fetch.
// Both halves are pipeline concerns, which is why it could not stay in the
// storage layer.
pub mod epg;
pub mod fetched_playlist;
pub mod geoip;
pub mod input_cache;
pub mod metadata_sink;
pub mod parser;
pub mod playlist_watch;
pub mod processor;
