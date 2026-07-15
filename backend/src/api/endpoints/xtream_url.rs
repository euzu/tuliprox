//! Shared URL-building helpers for the xtream and HLS code paths.
//!
//! Three symbols (`ApiStreamContext`, `get_query_path`,
//! `get_xtream_player_api_stream_url`) historically live in `xtream_api.rs`
//! but both `xtream_api` and `hls_api` need them. Importing them directly from
//! `xtream_api` creates a visible cross-import between two sibling endpoint
//! files, which is the symptom of a missing service layer. Until the full
//! `services::hls::playback` extraction lands (see ARCH-1 roadmap below),
//! route the cross-import through this re-export module so:
//!
//! - `hls_api` no longer depends on a sibling endpoint file directly.
//! - The cross-dependency is one-way and visible at a single import site.
//! - Future extraction only has to move this file's contents, not chase
//!   cross-imports across the `hls_api` surface.
//!
//! # ARCH-1 roadmap (deferred)
//!
//! The full `ARCH-1` fix extracts a `services::hls::playback` module that owns
//! the auth, admission, session lookup, lease, cache serve, and fallback
//! pipeline. `hls_api.rs` then becomes a thin Axum handler layer. That
//! extraction touches about 50 imports and 30 functions; it is intentionally
//! deferred from this commit because the change requires concurrent test
//! re-orchestration for every state-machine branch in the cache, and any
//! partial move is more dangerous than a single atomic commit. This
//! re-export module is the smallest step that unblocks the dependency
//! direction without that risk.

pub(in crate::api) use super::xtream_api::{get_query_path, get_xtream_player_api_stream_url, ApiStreamContext};