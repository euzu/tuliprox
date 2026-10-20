//! Media-server anti-corruption layer.
//!
//! This module keeps Emby/Jellyfin/Plex wire DTOs and transport concerns at the
//! Source Acquisition boundary. Playlist Curation and Stream Brokerage should
//! depend on the typed concepts exported here instead of provider-specific DTOs.

pub mod any_client;
pub mod catalog;
pub mod client;
pub mod emby;
pub mod errors;
pub mod jellyfin;
pub(crate) mod library_selection;
pub(crate) mod operation;
pub mod playback;
pub mod playlist_mapper;
pub mod plex;
pub mod redaction;
pub mod types;

#[cfg(test)]
pub mod test_fixtures;
#[cfg(test)]
pub(crate) mod test_server;

use std::{fmt::Write as _, sync::Arc};

/// Trimmed, shared copy of `value`, or `None` when it is absent or blank.
pub(crate) fn non_blank_arc(value: Option<&str>) -> Option<Arc<str>> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(Arc::<str>::from)
}

/// Percent-encode a single URL path segment using the unreserved-character
/// rules of [RFC 3986 §2.3]: alphanumeric, `-`, `.`, `_`, `~` are passed
/// through; every other byte becomes `%XX`.
///
/// Used by the Plex client (when assembling media URLs) and by the internal
/// playlist mapper (when building `/playlist/mapper/...` lookup paths) — both
/// need identical percent-encoding semantics for path segments.
pub fn encode_url_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

pub use any_client::*;
pub use catalog::*;
pub use client::*;
pub use errors::*;
pub use playback::*;
pub use playlist_mapper::*;
pub use redaction::*;
pub use types::*;
