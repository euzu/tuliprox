//! Shared URL-building helpers for the xtream and HLS code paths.
//!
//! `ApiStreamContext`, `get_query_path`, and
//! `get_xtream_player_api_stream_url` live in `xtream_api.rs`, while both the
//! Xtream and HLS handlers use them. This module keeps that dependency one-way
//! and gives callers a single stable import boundary:
//!
//! - `hls_api` no longer depends on a sibling endpoint file directly.
//! - The cross-dependency is one-way and visible at a single import site.

pub(in crate::api) use super::xtream_api::{get_query_path, get_xtream_player_api_stream_url, ApiStreamContext};
